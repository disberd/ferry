---
name: ferry
description: Use to list, read or send files in the user's Exchange folder on their Workstation (ferry ls, pull, push).
---

# ferry

The user's Workstation has an Exchange folder. You can list it, read files from it and write files to it with the `ferry` command. You cannot read other files on the Workstation, and you cannot run anything there.

## Commands

- `ferry ls [dir]`: list a folder of the Exchange folder.
- `ferry pull <xpath> [local]`: copy a file from the Exchange folder. With no `local`, ferry saves it in `~/.local/state/ferry/in/` and prints the path. Use `-` to write to stdout.
- `ferry push <local> [xpath]`: send a file to the Exchange folder. ferry makes the parent folders. If the file is there already, ferry replaces it and keeps one previous copy.

An `xpath` is a path relative to the Exchange folder, with `/` between folders. There is no command to delete, move or make a folder.

## Errors

ferry writes one line to stderr and exits with one of these codes:

- 2: bad path (absolute, `..`, outside the folder, or a name that Windows cannot hold).
- 3: the Workstation is not connected. Tell the user which file you did not send or read. Do not try again yourself.
- 4: the file is open in a program on the Workstation (for example Excel). Tell the user to close it.
- 5: the file is too large.
