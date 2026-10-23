-- Real-app Playwright fixture: one admin user with three disabled-for-IMAP accounts and four
-- conversations, including the "golden" one (5 logical messages, 6 physical copies across 4
-- folders). Idempotent: the user's mail data is replaced on every run. Run after the server has
-- applied its migrations:
--
--   psql -v ON_ERROR_STOP=1 -v username=playwright@example.test -v password='PlaywrightPassword123!' \
--     -f backend/tests/e2e/playwright-seed.sql

CREATE EXTENSION IF NOT EXISTS pgcrypto;

SELECT set_config('seed.username', :'username', false), set_config('seed.password', :'password', false);

-- One physical copy of a logical message.
CREATE FUNCTION pg_temp.seed_copy(p_account uuid, p_uid int, p_folder text, p_logical uuid, p_conv uuid, p_user uuid,
                                  p_canonical text, p_subject text, p_body text, p_direction text, p_days int)
RETURNS void LANGUAGE sql AS $fn$
  INSERT INTO messages (account_id, uid, folder, message_id, subject, from_name, from_email, to_addresses, date, snippet,
                        body_text, body_html, is_read, thread_id, logical_message_id, conversation_id,
                        conversation_user_id, canonical_message_id, threading_reason, threading_confidence)
  VALUES (p_account, p_uid, p_folder, p_canonical, p_subject,
          CASE WHEN p_direction = 'outgoing' THEN 'Ja' ELSE 'Alice' END,
          CASE WHEN p_direction = 'outgoing' THEN 'me@gmail.test' ELSE 'alice@example.test' END,
          CASE WHEN p_direction = 'outgoing'
               THEN '[{"address": "alice@example.test", "name": "Alice"}]'::jsonb
               ELSE '[{"address": "me@gmail.test", "name": "Ja"}]'::jsonb END,
          NOW() - make_interval(days => p_days), p_subject, p_body,
          format('<p>%s</p><blockquote>Quoted previous message</blockquote>', p_body), true, p_conv::text, p_logical,
          p_conv, p_user, p_canonical, 'playwright-fixture', 1)
$fn$;

DO $seed$
DECLARE
  v_username text := current_setting('seed.username');
  v_prefs jsonb := '{"conversation_list_view_enabled": true, "conversation_reader_view_enabled": true}';
  v_user uuid;
  v_accounts uuid[] := '{}';
  v_account uuid;
  v_gmail uuid;
  v_conv uuid;
  v_golden uuid;
  v_logical uuid;
  v_canonical text;
  v_subject text;
  v_count int;
  v_direction text;
  v_golden_dirs text[] := ARRAY['incoming', 'outgoing', 'incoming', 'outgoing', 'incoming'];
  v_golden_copies text[][] := ARRAY[['INBOX', NULL], ['Sent', NULL], ['Archive', NULL], ['Sent', NULL], ['INBOX', 'All-Mail']];
  v_uid int := 1001;
  v_folder text;
  v_phys int;
  v_folders int;
  v_logical_count int;
  rec record;
BEGIN
  -- bcrypt cost 4: fast, and verified by the server like any other bcrypt hash.
  INSERT INTO users (username, password_hash, is_admin, preferences)
  VALUES (v_username, crypt(current_setting('seed.password'), gen_salt('bf', 4)), true, v_prefs)
  ON CONFLICT (username) DO UPDATE
    SET password_hash = EXCLUDED.password_hash, is_admin = true,
        preferences = COALESCE(users.preferences, '{}'::jsonb) || v_prefs
  RETURNING id INTO v_user;

  DELETE FROM messages WHERE account_id IN (SELECT id FROM email_accounts WHERE user_id = v_user);
  DELETE FROM conversations WHERE user_id = v_user;
  DELETE FROM email_accounts WHERE user_id = v_user;

  -- Gmail (with All-Mail), Outlook, Fastmail. E2E_DISABLE_IMAP_CONNECT keeps them offline.
  FOR rec IN SELECT * FROM (VALUES
      (1, 'Gmail fixture', 'me@gmail.test', 'imap.gmail.com'),
      (2, 'Outlook fixture', 'me@outlook.test', 'outlook.office365.com'),
      (3, 'Fastmail fixture', 'me@fastmail.test', 'imap.fastmail.com')) AS a(n, name, email, host) ORDER BY n
  LOOP
    INSERT INTO email_accounts (user_id, name, email_address, auth_user, imap_host, enabled, protocol)
    VALUES (v_user, rec.name, rec.email, rec.email, rec.host, true, 'imap')
    RETURNING id INTO v_account;
    v_accounts := v_accounts || v_account;
    INSERT INTO folders (account_id, path, name, total_count, unread_count) VALUES (v_account, 'INBOX', 'INBOX', 0, 0);
  END LOOP;
  v_gmail := v_accounts[1];

  -- Golden dataset: one Gmail conversation, all copies in one native thread.
  v_subject := 'Golden conversation thread';
  INSERT INTO conversations (user_id, account_id, canonical_subject, subject_snapshot, first_message_at, last_message_at,
                             logical_message_count, copy_count, unread_count, threading_confidence)
  VALUES (v_user, v_gmail, v_subject, v_subject, NOW() - interval '5 days', NOW(), 5, 6, 0, 1)
  RETURNING id INTO v_golden;
  FOR i IN 0..4 LOOP
    v_canonical := format('<fixture-%s-%s@example.test>', left(v_golden::text, 8), i);
    INSERT INTO logical_messages (user_id, account_id, conversation_id, canonical_message_id, raw_message_id, subject,
                                  canonical_subject, direction, message_date, threading_reason, threading_confidence)
    VALUES (v_user, v_gmail, v_golden, v_canonical, v_canonical, v_subject, v_subject, v_golden_dirs[i + 1],
            NOW() - make_interval(days => 5 - i), 'playwright-fixture', 1)
    RETURNING id INTO v_logical;
    FOREACH v_folder IN ARRAY v_golden_copies[i + 1:i + 1] LOOP
      CONTINUE WHEN v_folder IS NULL;
      PERFORM pg_temp.seed_copy(v_gmail, v_uid, v_folder, v_logical, v_golden, v_user, v_canonical, v_subject,
                                format('Fixture body %s', i + 1), v_golden_dirs[i + 1], 5 - i);
      v_uid := v_uid + 1;
    END LOOP;
  END LOOP;

  -- Other conversations, for variety.
  FOR rec IN SELECT * FROM (VALUES (0, 'Gmail reply chain'), (1, 'Outlook conversation'), (2, 'Fastmail generic IMAP')) AS c(idx, subject) ORDER BY idx
  LOOP
    v_count := CASE WHEN rec.idx = 0 THEN 3 ELSE 2 END;
    v_account := v_accounts[rec.idx % 3 + 1];
    INSERT INTO conversations (user_id, account_id, canonical_subject, subject_snapshot, first_message_at, last_message_at,
                               logical_message_count, copy_count, unread_count, threading_confidence)
    VALUES (v_user, v_account, rec.subject, rec.subject, NOW() - interval '5 days', NOW(), v_count, v_count, 0, 1)
    RETURNING id INTO v_conv;
    FOR n IN 0..v_count - 1 LOOP
      v_direction := CASE WHEN rec.idx = 0 AND n = v_count - 1 THEN 'outgoing' ELSE 'incoming' END;
      v_canonical := format('<fixture-%s-%s@example.test>', left(v_conv::text, 8), n);
      INSERT INTO logical_messages (user_id, account_id, conversation_id, canonical_message_id, raw_message_id, subject,
                                    canonical_subject, direction, message_date, threading_reason, threading_confidence)
      VALUES (v_user, v_account, v_conv, v_canonical, v_canonical, rec.subject, rec.subject, v_direction,
              NOW() - make_interval(days => 5 - n), 'playwright-fixture', 1)
      RETURNING id INTO v_logical;
      PERFORM pg_temp.seed_copy(v_account, 2000 + rec.idx * 10 + n, 'INBOX', v_logical, v_conv, v_user, v_canonical,
                                rec.subject, format('Fixture body %s', n + 1), v_direction, v_count - n);
    END LOOP;
  END LOOP;

  UPDATE folders f
     SET total_count = (SELECT COUNT(*) FROM messages m WHERE m.account_id = f.account_id AND m.folder = f.path),
         unread_count = (SELECT COUNT(*) FROM messages m WHERE m.account_id = f.account_id AND m.folder = f.path AND m.is_read = false)
   WHERE f.account_id = ANY (v_accounts);

  SELECT COUNT(*) INTO v_count FROM conversations WHERE user_id = v_user;
  IF v_count <> 4 THEN
    RAISE EXCEPTION 'Playwright seed validation failed: expected 4 conversations, got %', v_count;
  END IF;
  SELECT c.logical_message_count, COUNT(m.*), COUNT(DISTINCT m.folder)
    INTO v_logical_count, v_phys, v_folders
    FROM conversations c LEFT JOIN messages m ON m.conversation_id = c.id
   WHERE c.id = v_golden GROUP BY c.logical_message_count;
  IF v_logical_count <> 5 OR v_phys <> 6 OR v_folders < 4 THEN
    RAISE EXCEPTION 'Golden dataset validation failed: logical=%, physical=%, folders=%', v_logical_count, v_phys, v_folders;
  END IF;
  RAISE NOTICE 'seeded user % (%), golden conversation %', v_username, v_user, v_golden;
END
$seed$;
