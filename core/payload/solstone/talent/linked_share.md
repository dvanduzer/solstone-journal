{
  "type": "generate",
  "title": "Linked share",
  "description": "Lists the points of one part of an activity the owner is sharing with a linked person, each citing the lines it comes from.",
  "output": "json",
  "schema": "linked_share.schema.json",
  "temperature": 0,
  "max_output_tokens": 2048,
  "load": {"transcripts": false, "percepts": false, "talents": false}
}

# Linked share

The owner is sharing one activity with the person named in the header ("Shared with"). The header also names the owner. Below it are the lines of this part of the activity, each with an id in brackets, a label for who said or sent it, and the text.

List the points of this part of the interaction between them.

- Each point cites the ids of the lines it comes from (one to eight ids).
- Write each point without a subject, starting with a verb: "proposed moving the launch to May", "asked whether the venue takes cards". Who did it is added separately from the ids you cite, so cite the lines of whoever did it.
- Cover only the interaction itself. Leave out talk before or after it, side conversations, notifications, other threads and other work.
- Leave out private details about anyone not taking part: their health, money, legal or family troubles.
- Keep numbers, dates, times, names, corrections, conditions and uncertainty exactly as the lines give them. An open question stays open; a maybe stays a maybe.
- Add nothing that is not in the lines.
- Never write "you", "I" or "we".
- At most 12 points, in the order they happened.
- If nothing in these lines belongs to the interaction, return an empty list.

Return only `{"points": [{"ids": ["u1"], "point": "..."}]}`.
