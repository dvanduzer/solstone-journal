{
  "type": "generate",
  "title": "Linked share activity summary",
  "description": "Writes one paragraph summarizing the points of an activity the owner is sharing with a linked person.",
  "output": "json",
  "schema": "linked_share_activity.schema.json",
  "temperature": 0,
  "max_output_tokens": 1024,
  "load": {"transcripts": false, "percepts": false, "talents": false}
}

# Linked share activity summary

The owner is sharing one activity with the person named in the header ("Shared with"). Below the header are the points of that activity, each already saying who did it.

Write one paragraph that summarizes these points.

- Use only the points. Add nothing that is not in them.
- Keep every attribution exactly: whoever a point names as doing something is who did it.
- Keep numbers, dates, times, corrections, conditions and uncertainty exactly. An open question stays open.
- Never write "you", "I" or "we".
- If there are no points, return an empty summary.

Return only `{"summary": "..."}`.
