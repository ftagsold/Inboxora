SELECT
  1::int2 AS i2, 2147483647::int4 AS i4, 9007199254740993::int8 AS i8, 26::oid AS oid,
  1.5::float4 AS f4, 0.1::float8 + 0.2::float8 AS f8, 3::float8 AS f8_int, 'NaN'::float8 AS f8_nan,
  12.50::numeric(10,2) AS num, (-0.000120)::numeric AS num_small, 1e20::numeric AS num_big,
  true AS b, 'héllo'::text AS t, 'x'::varchar(3) AS vc, 'ab'::char(4) AS bpc,
  'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid AS u,
  '{"b":1,"a":[1,2,{"z":null}]}'::json AS j, '{"b":1,"a":2}'::jsonb AS jb,
  '2026-10-22 08:59:26.109876+02'::timestamptz AS ts, '2026-10-22 08:59:26.5'::timestamp AS ts_local,
  '2026-10-22'::date AS d, 'infinity'::timestamptz AS ts_inf,
  '\x0102ff'::bytea AS bin, '192.168.0.1'::inet AS ip, '10.0.0.0/8'::cidr AS net, '10.1.2.3/16'::inet AS ip_mask,
  ARRAY['a','b',NULL]::text[] AS ta, ARRAY[1,2]::int4[] AS ia, ARRAY[5]::int8[] AS i8a,
  ARRAY['a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11']::uuid[] AS ua, ARRAY[[1,2],[3,4]]::int4[] AS ia2,
  '{}'::text[] AS empty_arr, NULL::text AS n, '1 day 02:03:04.5'::interval AS iv,
  'x'::name AS nm
