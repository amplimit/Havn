-- What a reply's turn did on the way (issue #178): the model's reasoning
-- and the tool calls with their results, as JSON; shown in the dashboard,
-- never sent to the model again.
ALTER TABLE conversations ADD COLUMN trace TEXT;
