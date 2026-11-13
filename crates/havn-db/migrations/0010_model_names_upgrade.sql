-- Gateways before the model list stored model names without a provider
-- (`claude-opus-4-7` rather than `anthropic:claude-opus-4-7`). A row here
-- marks a database that may still hold such names: the gateway rewrites
-- them once at start and deletes the row. A database this migration
-- creates empty never gets one.
CREATE TABLE model_names_upgrade (
    id INTEGER NOT NULL PRIMARY KEY CHECK (id = 1)
) STRICT;

INSERT INTO model_names_upgrade (id)
SELECT 1
WHERE EXISTS (SELECT 1 FROM agents)
   OR EXISTS (
        SELECT 1 FROM roles
        WHERE CASE WHEN json_valid(policy)
                   THEN coalesce(json_extract(policy, '$.allowed_models'), '["*"]')
                   ELSE '["*"]'
              END <> '["*"]'
   );
