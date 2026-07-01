# Simple Cursor Locker by FPSHEAVEN

A compact Windows Rust utility that detects your monitors and confines the cursor to one selected monitor. The UI is built with egui and the cursor lock uses native Win32 calls.

![Simple Cursor Locker screenshot](assets/screenshot.png)

## Build

```powershell
cargo build --release
```

The optimized app is written to:

```text
target\release\screen_locker.exe
```

## Usage

1. Open `target\release\screen_locker.exe`.
2. Pick a monitor from the layout.
3. Click `Set key` and press the key or shortcut you want to use.
4. Press that key to lock or unlock the cursor.

The bind is polled without reserving the key, so it still reaches Windows and the foreground app. Single-key binds are allowed, but they will lock or unlock the cursor whenever that key is pressed.

Emergency unlock:

`Ctrl+Alt+Esc` when Windows allows the app to register it. This is the only reserved Windows hotkey. If another app owns it, set a lock/unlock key before locking.

Use the lock/unlock key or emergency unlock before interacting outside the locked monitor.

Settings are saved to:

```text
%LOCALAPPDATA%\screen_locker\settings.ini
```

The app unregisters hotkeys and releases the cursor lock when it closes.

Build and run in one command:

```powershell
cargo build --release; .\target\release\screen_locker.exe
```
