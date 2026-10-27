# ferry

ferry connects a Windows Workstation to a Linux Remote over the `ssh.exe` of Windows. It has two parts:

- A hotkey (Ctrl+Alt+V) on the Workstation sends the Screenshot or the files on the clipboard to the Remote. ferry types their Remote paths into the focused herdr pane.
- Agents on the Remote list, read and write files in the Exchange folder of the Workstation (`%USERPROFILE%\ferry\`) with `ferry ls`, `ferry pull` and `ferry push`.

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

## Use

- `ferry start <alias>` starts ferry in the background. `ferry stop` stops it. `ferry status` shows its state.
- The log is `%LOCALAPPDATA%\ferry\ferry.log`.

## Upgrade

1. Workstation: `ferry stop`, `scoop update ferry`, `ferry start <alias>`. Scoop does not update ferry while it runs.
2. Remote: run `install.sh` again. It stops the old `ferry serve`, and the Workstation connects again.

## License

Apache-2.0. See `LICENSE`.
