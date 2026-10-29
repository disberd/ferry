# ferry

ferry connects a Windows Workstation to a Linux Remote over the `ssh.exe` of Windows. It has three features:

- A hotkey (Ctrl+Alt+V) on the Workstation sends the Screenshot or the files on the clipboard to the Remote. ferry types their Remote paths into the focused herdr pane.
- Agents on the Remote list, read and write files in the Exchange folder of the Workstation (`%USERPROFILE%\ferry\`) with `ferry ls`, `ferry pull` and `ferry push`.
- The Workstation plays a sound when a herdr agent is Blocked (it needs input) or Done.

ferry has no internet access and does not update itself.

Versions before 0.1.0 are test builds of the release path. Do not use them for real work.

## Install

On the Remote:

```sh
curl -fsSL https://raw.githubusercontent.com/disberd/ferry/main/install.sh | sh
```

The script puts `ferry` in `~/.local/bin/`, the agent skill in `~/.agents/skills/ferry/`, and links a herdr plugin. Read the script before you run it.

On the Workstation, with [scoop](https://scoop.sh):

```powershell
scoop bucket add custom-scoop https://github.com/disberd/custom-scoop
scoop install ferry
```

With no scoop, download `ferry-windows-x86_64.exe` from the release page and compare its hash with `SHA256SUMS`.

The Workstation must log in to the Remote with no prompt: `ssh.exe -o BatchMode=yes <alias> true` must succeed. `<alias>` is the name of the Remote in `~/.ssh/config`.

## Start at login

This PowerShell command adds a shortcut to the Startup folder. Replace `<alias>`.

```powershell
$s = (New-Object -ComObject WScript.Shell).CreateShortcut("$([Environment]::GetFolderPath('Startup'))\ferry.lnk")
$s.TargetPath = "$(scoop prefix ferry)\ferry.exe"
$s.Arguments = "start <alias>"
$s.WindowStyle = 7
$s.Save()
```

On Windows 11, pin the ferry tray icon once. Until you pin it, Windows keeps it in the overflow area.

## Use

- `ferry start <alias> [--hotkey <key>] [--apps <exe>,<exe>]` starts ferry in the background. `ferry stop` stops it. `ferry status` shows its state.
- The log is `%LOCALAPPDATA%\ferry\ferry.log`.

### Options

- `--hotkey` sets the key. The default is `Ctrl+Alt+V`. A value is the modifiers `Ctrl`, `Alt`, `Shift` and `Win`, joined by `+`, then one letter, one digit, or `F1` to `F24`.
- `--apps` is a comma-separated list of exe names. The default is `rio.exe`. Case is not important.
- The hotkey works only while an app of the `--apps` list has the focus. In other apps, the key keeps its normal use.

### Hotkey

- A press sends the files on the clipboard. With no files, it sends a PNG image. A folder in the set, or an image with no PNG format, gives the error sound.
- With no herdr client attached, the hotkey types into the last focused pane. No sound plays and the clipboard stays as it was.
- With two herdr clients attached, the paste can go to the wrong pane. Move to a pane in the client that you want before you press the hotkey.
- ferry uses only the default herdr session. Named herdr sessions are not supported.
- Alerts are lost while no `ferry serve` runs on the Remote.

### Files that agents push

Files that agents push carry the Mark of the Web. Office opens them in Protected View. If an IT policy blocks a pushed exe, run `Unblock-File <path>` in PowerShell, or choose "Unblock" in the file Properties.

## Upgrade

Upgrade the Workstation first. An old test build on either side is a version mismatch: ferry retries, and `ferry status` names the older side.

1. Workstation: `ferry stop`, `scoop update ferry`, `ferry start <alias>`. Scoop does not update ferry while it runs.
2. Remote: run `install.sh` again. It stops the old `ferry serve`, and the Workstation connects again in 60 s at most.

If `scoop update ferry` finds no new version after a release, run the bucket workflow by hand: `gh workflow run update.yml -R disberd/custom-scoop`.

## License

Apache-2.0. See `LICENSE`.
