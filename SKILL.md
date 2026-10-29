---
name: ferry
description: Use when the user mentions their "Exchange folder", says "send me" or "put it on my machine", or asks for "my file X". Lists, reads and sends files in the Exchange folder on their Workstation (ferry ls, pull, push).
---

# ferry

The user's Workstation has an Exchange folder. It is the only part of the Workstation that you can reach. Use `ferry` to list the folder, read its files and write files to it. You cannot delete, move or run anything there.

## Commands

- `ferry ls [dir]`: list a folder. Each line is `size<TAB>mtime<TAB>path`. A folder has a trailing `/`.
- `ferry pull <xpath> [local]`: copy a file from the Exchange folder. With no `local`, ferry saves it in `~/.local/state/ferry/in/` and prints the path. Use `-` to write the bytes to stdout.
- `ferry push <local> [xpath]`: send a file to the Exchange folder. ferry makes the parent folders and prints `saved: <xpath>`. If the file is there already, ferry replaces it and keeps one previous copy.

An `xpath` is a path relative to the Exchange folder, with `/` between folders. There is no command to delete, move or make a folder.

## Rules

- Use `push` to give the user a file.
- A path under `~/.local/state/ferry/in/` came from the Workstation (a hotkey press or a `pull`). Read it there.
- To edit a file that the user put in the Exchange folder, push the edit under a new name, for example `report-v2.xlsx`. Overwrite the file only when the user asks. ferry keeps one previous copy, so a second push loses the original.
- A hotkey paste goes into the last focused herdr pane, also when no herdr client shows that pane. The user may not see it.
- Under the Codex `workspace-write` sandbox, each `ferry` call needs an approval.

## Errors

`ls`, `pull` and `push` write one line to stderr and exit with one of these codes:

- 1: other error. The line gives the cause.
- 2: bad path or bad command line. Examples: an absolute path, `..`, a refused file type, a missing path, or a folder instead of a file.
- 3: the Workstation is offline or the connection ended. Tell the user which file you did not send or read. Do not try again yourself.
- 4: the file is open in a program on the Workstation (for example Excel). Tell the user to close it.
- 5: the transfer is over 512 MiB, or the Workstation disk is low.
