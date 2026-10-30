# Site and rule fixture provenance

- Upstream source: `fffonion/xeHentai` `43fc55c0c662f195d048becf6276a26ce06b09d9`.
  `crates/xeh-xtask/src/site.rs` resolves the checkout under
  `reviews/task5-5af6f1e/upstream` and verifies its 19 package files against the
  pinned Git tree and blobs before any case runs.
- Every request, response, header and callback sentinel in `cases.json` is
  synthetic. `compat/oracle/site_runner.py` disables `socket.socket.connect` and
  `socket.create_connection`, runs in an owned case cwd and receives only the
  documented environment whitelist. No account, cookie, proxy or real site data
  is used.
- `cases.json` feeds both the RSS assertion suites
  (`cargo run -p xeh-xtask -- rss-test --suite site|rules`) and the fixed-source
  differential (`cargo run -p xeh-xtask -- diff --suite site`), which also runs
  nine mutation gates through the same production comparator.
- Suite sizes: 19 `site` cases and 17 `rules` cases (36 total).

## Branch coverage by case id

- `site.login.*`: two `Set-Cookie` captures, failure status/message, missing
  header fallback.
- `site.metadata.*`: status 600/404, fjords/banned prefixes, malformed fields,
  success names/total/tags/thumbnail count/MPV flags, entity titles and both MPV
  markup variants.
- `site.pages.*`: duplicate links and order, no-match fail with the raw
  `debug.htm` effect, status not generically checked.
- `site.mpv.*`: imagelist order and 1-based index, malformed list (two fails and
  continuation), strict-JSON `NaN` delta.
- `rule.image.quota-*`: 600 connection, 403 key, 509/size marker, equal-size
  exemption and mismatch, small-text/content-type threshold, last hash-size
  match, case-sensitive body text.
- `site.image.*`: keystamp over style fallback, `image.php?...n=` filename
  fallback, original/resampled URL triples, `nl`/redirect query rules,
  prefix-anchored `Invalid page`.
- `rule.download.*`: 404, zero length, missing callback, chunk callback before
  yield, empty chunk, iterator and callback connection aborts, short/long
  completion.
- `rule.support.*`: seven named entities, one-pass `&amp;lt;`, decimal numeric
  entities (Unicode `Nd`, lone surrogate and Python `int()` underscore deltas),
  human time with Unicode `Nd` digits.
