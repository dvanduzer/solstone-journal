# Source passages

Source passages are exact, bounded copies of an activity's source units: the
source an activity talent reads, split and lightly normalized as described
below. They keep the source's own wording, so whatever attribution, condition,
negation or tense a passage holds is the source's own: "Lee could deploy v14
only after both suites pass" is never reworded into "Lee deployed v14".

A passage is source text copied verbatim, with its offsets and a hash of the
text it came from. It is not a verified fact, it names no speaker the source
did not name, and a UI role or an absent label never establishes who someone
is.

Nothing reads passages by default, and Work and Conversation Stories do not
use them. Passages are a separate capability for a caller that wants
source-faithful material from an activity.

## The command

```bash
solstone journal talent passages <name> --day YYYYMMDD --facet NAME --activity ID [--json]
```

`<name>` is an activity talent such as `work` or `conversation`. The command
prepares the activity the way that talent's run would, then copies passages
from the source it would read. It writes no passages to the journal. A
completion it asks for goes through the generate boundary like any other: the
owner's chosen model, lane and thinking setting apply, and its usage goes into
the journal's usage log.

Without `--json` it prints each passage under a `Verbatim from source:` line.
With `--json` it prints the passage document below. The exit status is `0`
when every unit has its passages and `1` when any unit does not, in which case
the document is still printed, so each unit says what happened to it. An
activity that cannot be prepared also exits `1`, with an error and no
document; bad arguments exit `2`.

## How passages are chosen

1. **Units.** The activity's rendered transcript is split by segment and by
   channel (`Transcript`, `Screen Activity`, `Browser Content` and the
   import channels). Each unit's text starts with the activity type, the
   segment heading and the channel name. A fenced block holding one strict JSON
   value becomes, between `JSON values begin` and `JSON values end` lines, a
   path-and-type line per scalar followed by its value; anything else,
   including a fence that does not parse or repeats a key, stays exactly as
   written. A unit that would pass 16,000 characters, its heading lines
   included, is split into chunks that overlap by 256 characters.
2. **A unit that fits is kept whole.** When `Verbatim from source:\n` plus the
   whole unit fits in 3,840 characters, the unit is its own passage and no
   model is called.
3. **A longer unit takes one completion.** The model nominates up to 12
   (audio, and screen for `work`) or 8 short notes about what is worth
   keeping: 1,536 visible output tokens, temperature 0.3, a strict JSON
   schema. Its notes are then discarded.
4. **Code copies the source.** Each note is located in the unit from shared
   distinctive words alone. A note that matches too little, or would overrun
   the budget, is counted as rejected. Located spans are widened to the nearest
   line or sentence boundary, by at most 256 characters on each side, while all
   of the unit's passages together still fit in 3,840 characters, prefixes
   included. Every located span stays inside some passage.

There is no judge, no quality retry and no second model pass.

## The passage document

```json
{
  "schema": "solstone-source-passages-v1",
  "talent": "conversation", "day": "20260101", "facet": "work", "activity": "…",
  "complete": true,
  "offset_unit": "utf8_byte",
  "budget_unit": "unicode_scalar",
  "passage_prefix": "Verbatim from source:\n",
  "max_rendered_chars": 3840,
  "units": [
    {
      "unit": 0, "source_kind": "audio", "timestamp": "…", "segment": 1, "channel": 1, "chunk": 1,
      "source_sha256": "…", "source_bytes": 6731, "source_chars": 6589,
      "outcome": "selected", "nominations": 4, "rejected_nominations": 0, "call": 0,
      "rendered_chars": 1164,
      "passages": [{"start": 2523, "end": 2904, "text": "…"}, "…"]
    }
  ],
  "calls": {
    "logical": 1, "physical": 1, "with_usage": 1, "without_usage": 0,
    "usage": {"input_tokens": 2710, "output_tokens": 92, "total_tokens": 2802},
    "completions": [{"units": [0], "usage": {"…": "…"}, "model": "…"}]
  }
}
```

- `text` is the unit's text from byte `start` to byte `end`, and
  `source_sha256` is the SHA-256 of the unit's UTF-8 text. Budgets count
  Unicode scalar values.
- `outcome` is `whole` (no call), `selected`, `failed` or `not_attempted`.
  `selected` with `nominations: 0` is a valid empty selection: the model found
  nothing worth keeping, which is a success with no passages.
- `failed` carries `failure.kind`: `refused`, `incomplete` (the completion did
  not finish), `clipped_input` (the model saw less than the whole unit),
  `schema_invalid`, `malformed` or `unavailable`. A failed unit has no
  passages; nothing from that completion is used.
- A blocking refusal, or a boundary that cannot be reached, stops the run
  asking for more completions. The long units after it are `not_attempted`.
  Units that fit whole are still kept.
- `calls.physical` counts completions actually made. `calls.logical` counts
  units served by one: identical units share a completion. A completion that
  does not report all three token counts, a refusal included, counts toward
  `without_usage` and adds nothing to the totals, so a total is a floor
  whenever `without_usage` is above zero. Its usage, as reported, stays in
  `completions`.

## Using it from Rust

`solstone_core_talent_runtime::source_passages` exposes the same steps:
`source_units(transcript, activity_kind)`, `extract(&units, talent, &generate)`
with any `GenerateExecutor`, and `document(...)` or `readable(...)` for output.
`activity_passages` is the whole command behind one call. `whole_passage`,
`nomination_request` and `select_passages` are the per-unit steps, for a
caller that sends completions itself.

## The conformance cases

`core/crates/solstone-core-talent-runtime/src/source_passages/conformance.json`
holds invented cases, which the routine unit tests run either through the
command's entry point, from an `audio` journal fixture, or straight from a
`transcript`. Each case can name:

- `references`: text that must stay together. A reference is protected only
  when all of its fragments appear in one unit and it is marked useful. It
  counts as kept only when one passage holds all of its fragments, so an
  action kept without its speaker or condition counts as lost.
- `qualified`: a claim that may appear in a passage only with every one of
  its qualifiers.
- `forbidden`: stronger or reassigned readings that no passage may contain.
- `expect`: physical and logical calls, completeness and each unit's outcome.

The harness also checks every passage against its unit's offsets, hash and
budget. Add a case whenever a passage regression is found. Use invented
material only.

## Known limits

- Passages are longer than a written summary, often several times the model's
  notes for the same unit. Whatever reads them pays for that.
- Locating is lexical and its stop words are English. A note that paraphrases
  away from the source's words can be rejected; the rejection is counted, never
  guessed around.
- Passages say nothing about how well a Story written from them would read,
  and no Story uses them.
