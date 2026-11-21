-- A task's review is now the capture of the stage that `payload.review_stage`
-- names, not whichever stage's capture first holds a `/pull/` URL. Point
-- tasks that already have a pull-request link at the stage the old query
-- would have picked, once, so their link survives the upgrade.
--
-- Only rows with no `review_stage` key (a key holding null or any other value
-- is someone's choice) and an object `stages`. The first stage in stored
-- order whose object capture's `url` matches LIKE '%/pull/%' is the old
-- query's pick, and it is frozen *before* it is validated: a first pick that
-- is not recognized leaves the row alone and never falls through to a later
-- one. Recognized: after at most one trailing slash, the url ends, case
-- sensitively, in `/pull/` and 1-18 ASCII digits that make a positive number.
-- A pick whose key contains a double quote is skipped as well.
--
-- Changes exactly three facts: `review_stage`, `stages.<key>.number` (the
-- URL's, over any captured one) and `stages.<key>.label` (`#<number>`). The
-- stage path is built once with json_quote so a key holding a dot, a
-- backslash or a newline addresses that stage and cannot create another.
WITH first AS (
    SELECT w.task_id AS task_id,
           (SELECT json_object('k', j.key, 'u', json_extract(j.value, '$.url'))
              FROM json_each(w.payload, '$.stages') j
             WHERE j.type = 'object'
               AND json_extract(j.value, '$.url') LIKE '%/pull/%'
             LIMIT 1) AS pick
      FROM workflow_state w
     WHERE json_type(w.payload, '$.review_stage') IS NULL
       AND json_type(w.payload, '$.stages') = 'object'
),
trimmed AS (
    SELECT task_id,
           json_extract(pick, '$.k') AS key,
           CASE WHEN substr(json_extract(pick, '$.u'), -1) = '/'
                THEN substr(json_extract(pick, '$.u'), 1,
                            length(json_extract(pick, '$.u')) - 1)
                ELSE json_extract(pick, '$.u')
           END AS url
      FROM first
     WHERE pick IS NOT NULL
       AND json_type(pick, '$.u') = 'text'
       AND instr(json_extract(pick, '$.k'), '"') = 0
),
recognized AS (
    SELECT task_id,
           key,
           CAST(substr(url, length(rtrim(url, '0123456789')) + 1) AS INTEGER) AS number,
           '$.stages.' || json_quote(key) AS path
      FROM trimmed
     WHERE substr(rtrim(url, '0123456789'), -6) = '/pull/'
       AND length(url) - length(rtrim(url, '0123456789')) BETWEEN 1 AND 18
       AND CAST(substr(url, length(rtrim(url, '0123456789')) + 1) AS INTEGER) > 0
)
UPDATE workflow_state
   SET payload = (SELECT json_set(workflow_state.payload,
                                  '$.review_stage', r.key,
                                  r.path || '.number', r.number,
                                  r.path || '.label', '#' || r.number)
                    FROM recognized r
                   WHERE r.task_id = workflow_state.task_id)
 WHERE task_id IN (SELECT task_id FROM recognized);
