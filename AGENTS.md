# Agent Guidelines for gitmirrorcache

## Git command argument sanitization

Every method in `git-cache-git` that shells out to the `git` binary **must**
validate caller-supplied arguments before passing them to the command.
Unvalidated strings can be interpreted as flags (e.g. a value starting with `-`)
or carry embedded NUL bytes that truncate arguments.

### Rules

1. **Reject dangerous characters early.** Use the `reject_*` family of helpers
   (`reject_ref_arg`, `reject_revision_arg`, `reject_config_key`, etc.) at the
   top of every public method that forwards caller input to git. A helper should
   reject at minimum: empty strings, strings starting with `-`, and strings
   containing `\0`.
2. **Use `--` to separate flags from positional arguments** wherever git accepts
   it (e.g. `symbolic-ref -- <name> <target>`, `config --local -- <key> <value>`).
3. **Add specialised validators when the domain is narrower.** For example,
   config keys must never start with `-` but unlike refs they may contain `=`
   signs — use `reject_config_key` for config operations and `reject_ref_arg`
   for ref operations.
4. **Branch names, ref names, and revisions** each have their own validator.
   Pick the most specific one (`reject_ref_arg` checks for `:` which is invalid
   in refs; `reject_revision_arg` allows `:` because revisions like `HEAD:path`
   are valid).
5. **Never pass unvalidated user/network input to git** — this includes URL
   components, query parameters, and request body fields that end up as git
   arguments.

### Adding a new git wrapper method

When adding a new method to `Git`:

```text
1. Identify which arguments come from external input.
2. Choose or create the appropriate reject_* validator.
3. Call the validator before self.run(...).
4. Use `--` in the argument list where git supports it.
5. Add a test that verifies the method rejects a `-`-prefixed argument.
```
