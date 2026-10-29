# PR attachments

Upload regular files to a pull request, either while creating it or afterwards.

## `fj pr create --attach`

Create a pull request and attach files to it in one step:

```sh
fj pr create --attach ./screenshot.png --attach ./log.txt "Add the new feature"
```

- `--attach <FILE>` can be repeated. It cannot be combined with `--web` or
  `--agit`.
- Every file is checked before the pull request is created, so an unusable
  path never creates a pull request.
- The pull request is created first. Its number and URL are reported before
  the uploads, so a later failure never hides that the pull request exists.
  Without a URL from the server, the number and the `owner/repo` name are the
  recovery identifier.
- Files are uploaded one after another to the repository the pull request was
  created in, with the original basename as attachment name. With `--base ^`
  this is the parent repository of the head branch, with `-r` it is the
  selected repository.

### Local references in the body

A file can be placed where it belongs by referencing it in the body:

```markdown
Here is the new screen:

![the new screen](./screenshot.png)

And the [log](./log.txt) of the run.
```

`fj pr create --attach ./screenshot.png --attach ./log.txt` uploads both files
and rewrites the destinations of the references to the uploaded files. Only the
destination is replaced, the alt text, the title and everything else of the
body stay as they are.

- A relative reference is looked up next to the body file first and in the
  working directory afterwards. A body that is not read from a file uses the
  working directory.
- The file of a reference is the file of the uploaded attachment, compared by
  its identity on disk, so two files with the same name do not mix up.
- A reference that points to a file that was not uploaded is left alone.
- A reference inside a code block, inside inline code, inside raw HTML, in
  ordinary text, to another site or to an anchor of the page is left alone.
- A reference-style link or image is rewritten in its definition, so every use
  of it keeps its formatting. Only the definition that wins is rewritten: a
  line that looks like one inside a code block, inside raw HTML, after another
  non-definition line, or as a later duplicate of an earlier definition
  anywhere in the body is left alone. When the winning definition is left
  alone, the local reference stays and the uploaded file is appended.
- A definition on the same line as a list marker or a block quote marker
  (`- [label]: ./file.png`, `> [label]: ./file.png`), and a definition whose
  destination stands on the following line, are not rewritten: the local
  reference stays and the uploaded file is appended. Definitions on their own
  line, including inside a list item, can be rewritten when the match is safe.
- Reference labels spanning lines are left unchanged, and the file is appended.
  If the parser cannot safely determine which definition wins, the local
  reference also stays and the file is appended. Very long lines or many links
  can make that check inconclusive.
- A file that the body does not reference is appended to the end of the body,
  in the order of the arguments.

### Alt text

An argument can carry an alt text after a `#`:

```sh
fj pr create --attach "./screenshot.png#the new screen"
```

The file is the longest existing path before the `#`, the rest is the alt text.
A file name with a `#` in it keeps working as long as the path exists.

The alt text is used for a reference that is appended to the body. A reference
that the body already contains keeps the alt text of the body.

### Failures

- If an upload fails, the remaining files are still uploaded, the references of
  the successful uploads are added to the body, the failing file is reported with
  the reason of the failure and the command exits with a failure. Nothing is
  retried automatically.
- If the body cannot be read, no update is attempted: the generated links are
  printed for manual recovery and the command exits with a failure, because
  the uploads may already have succeeded.
- If the body cannot be updated, the generated links are printed for manual
  recovery and the command exits with a failure. The update may already have
  succeeded, so check the body before adding them again.

## `fj pr attach`

Upload regular files to an existing pull request and print Markdown links.
This command only prints the links, it does not change the pull request body.

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

## Limitations

- The link text is the attachment name. The `#alt` syntax of `gh` is only
  supported by `fj pr create --attach`.
- Only the destination of a matching reference is replaced, everything else of
  the body stays as it is. A reference that does not match an uploaded file is
  left alone and the file is appended instead.
- A destination with an escaped space is not a link for CommonMark, so such a
  reference stays text and the file is appended instead. Use the angle form
  `![alt](<my file.png>)` for a name with a space.
- The body update is a plain read followed by a write. A concurrent edit by
  somebody else can be overwritten, because the API has no compare-and-set
  for the body.
- Unlike `gh`, a failed upload does not stop the remaining files, the
  successful uploads are kept and the command reports the failure at the end.
