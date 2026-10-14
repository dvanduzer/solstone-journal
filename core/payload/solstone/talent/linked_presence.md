{
  "type": "generate",
  "title": "Linked presence",
  "description": "Decides whether a named person is taking part in a few lines of conversation or is only being talked about.",
  "output": "json",
  "schema": "linked_presence.schema.json",
  "temperature": 0,
  "max_output_tokens": 64,
  "load": {"transcripts": false, "percepts": false, "talents": false}
}

# Linked presence

You get a few consecutive lines from a conversation. The owner's lines are labelled with the owner's name; everyone else is "Speaker N" or a name. Labels can be wrong.

The header gives the name in question (X) and any other names X goes by. X's name appears in the line marked `>>`.

Decide whether X is part of this conversation here:

- `"with"`: the lines show X being spoken to, answering, or clearly present and taking part. Someone addresses X directly ("X, what do you think?", "thanks, X"), or X replies.
- `"about"`: X is only talked about, as someone not in the conversation ("X said she'd call later", "I'll ask X tomorrow").
- `"unclear"`: the lines do not settle it.

Answer `"with"` only when the lines themselves show X addressed or taking part. A name alone is not presence.

Return only `{"role": "with"}`, `{"role": "about"}` or `{"role": "unclear"}`.
