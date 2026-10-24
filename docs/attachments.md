# PR attachments

Upload regular files to an existing pull request and print Markdown links.

```sh
fj pr attach <PR> <FILE>...
fj pr attach owner/repo#7 ./screenshot.png ./log.txt
```

Each successful upload prints exactly one line:

```md
[filename](<https://forgejo.example/attachments/uuid>)
```

Behavior and limits:

- `<PR>` is required and must be a pull request number. Use `owner/repo#number`
  to select another repository. There is no current-branch inference.
- `<FILE>...` requires one or more regular files. `-` (stdin) is rejected.
  Directories, missing files, and names without a UTF-8 basename are rejected
  before any upload. Symlinks to regular files follow filesystem semantics.
  Repeated paths upload repeatedly.
- Files are read as binary and uploaded in order with the original basename
  sent as the attachment name. Output is flushed after each file. No extra
  progress text is printed and no comment is written automatically.
- If a later file fails, earlier links remain and the error identifies the
  failing file. A missing or non-`http(s)` download URL, or a failure writing
  output, is an error even when the upload itself succeeded.
