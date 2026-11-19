-- What a reply's turn said and did on the way to it (issues #212, #213):
-- the assistant messages with their text and tool calls, and the tool
-- results, as JSON. Given back to the model in later turns, so it knows
-- what it did; `content` stays the reply people read.
ALTER TABLE conversations ADD COLUMN steps TEXT;
