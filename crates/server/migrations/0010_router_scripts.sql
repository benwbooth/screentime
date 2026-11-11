-- The JavaScript that switches the router, editable in the web UI. NULL uses the built-in
-- script for the Netgear Nighthawk RS200.
ALTER TABLE wifi ADD COLUMN script_on TEXT;
ALTER TABLE wifi ADD COLUMN script_off TEXT;
ALTER TABLE wifi ADD COLUMN script_status TEXT;
