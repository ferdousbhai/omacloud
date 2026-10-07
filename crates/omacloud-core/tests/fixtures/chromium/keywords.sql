-- The keywords table as Chromium creates it (components/search_engines/
-- keyword_table.cc), with a built in engine, a starter pack shortcut, one
-- Chromium added from a site visited, and two the user made.
CREATE TABLE meta(key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
INSERT INTO meta VALUES('version','140'),('last_compatible_version','140');
CREATE TABLE keywords (id INTEGER PRIMARY KEY,short_name VARCHAR NOT NULL,keyword VARCHAR NOT NULL,favicon_url VARCHAR NOT NULL,url VARCHAR NOT NULL,safe_for_autoreplace INTEGER,originating_url VARCHAR,date_created INTEGER DEFAULT 0,usage_count INTEGER DEFAULT 0,input_encodings VARCHAR,suggest_url VARCHAR,prepopulate_id INTEGER DEFAULT 0,created_by_policy INTEGER DEFAULT 0,last_modified INTEGER DEFAULT 0,sync_guid VARCHAR,alternate_urls VARCHAR,image_url VARCHAR,search_url_post_params VARCHAR,suggest_url_post_params VARCHAR,image_url_post_params VARCHAR,new_tab_url VARCHAR,last_visited INTEGER DEFAULT 0, created_from_play_api INTEGER DEFAULT 0, is_active INTEGER DEFAULT 0, starter_pack_id INTEGER DEFAULT 0, enforced_by_policy INTEGER DEFAULT 0, featured_by_policy INTEGER DEFAULT 0, url_hash BLOB);
INSERT INTO keywords (short_name,keyword,favicon_url,url,safe_for_autoreplace,date_created,input_encodings,suggest_url,prepopulate_id,last_modified,sync_guid,alternate_urls,is_active) VALUES
 ('Example Search','example','https://search.example.com/favicon.ico','https://search.example.com/?q={searchTerms}',1,0,'UTF-8','https://search.example.com/suggest?q={searchTerms}',1,0,'aaaaaaaa-0000-4000-8000-000000000001','[]',1),
 ('Bookmarks','@bookmarks','','chrome://bookmarks/?q={searchTerms}',0,0,'',NULL,0,0,'aaaaaaaa-0000-4000-8000-000000000002','[]',1),
 ('shop.example.net','shop.example.net','https://shop.example.net/favicon.ico','https://shop.example.net/search?q={searchTerms}',1,13400000000000000,'',NULL,0,13400000000000000,'aaaaaaaa-0000-4000-8000-000000000003','[]',0),
 ('Example Wiki','w','https://wiki.example.org/favicon.ico','https://wiki.example.org/w/index.php?search={searchTerms}',0,13400000000000000,'UTF-8',NULL,0,13400000000000000,'aaaaaaaa-0000-4000-8000-000000000004','[]',1),
 ('Example Maps','m','','https://maps.example.com/?q={searchTerms}',0,13400000000000001,'',NULL,0,13400000000000001,'aaaaaaaa-0000-4000-8000-000000000005','[]',1);
UPDATE keywords SET starter_pack_id = 1 WHERE keyword = '@bookmarks';
