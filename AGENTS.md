# Agent Guidelines — gitmirrorcache

## Git argument sanitization

All `git-cache-git` methods that shell out to `git` **must** validate caller-supplied args. Unvalidated strings risk flag injection (`-` prefix) or NUL-byte truncation.

### Rules

1. **Validate early** — call `reject_*` helpers (`reject_ref_arg`, `reject_revision_arg`, `reject_config_key`, …) at the top of every public method forwarding input to git. Reject: empty strings, `-`-prefixed, `\0`-containing.
2. **Use `--`** to separate flags from positional args wherever git accepts it.
3. **Pick the narrowest validator** — `reject_config_key` for config (allows `=`), `reject_ref_arg` for refs (rejects `:`), `reject_revision_arg` for revisions (allows `:` for `HEAD:path`).
4. **Never pass unvalidated external input to git** — URLs, query params, request body fields included.

### New git wrapper checklist

1. Identify external-input args.
2. Choose/create appropriate `reject_*` validator.
3. Call validator before `self.run(…)`.
4. Add `--` where git supports it.
5. Test that `-`-prefixed input is rejected.
