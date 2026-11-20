# Corolla

![Corolla exploring a sample studio folder](assets/explorer.png)

Corolla is a native Rust disk explorer for Omarchy, modeled on DaisyDisk. Scan a disk or drop a folder into the window. Each ring shows another level of the directory tree. Wider segments take up more space. Hover to inspect a branch, click to zoom in, and click the center to go up one level.

## Run on Omarchy

Open **Corolla** from the launcher, run `corolla` for the disks overview, or pass a directory to scan immediately.

```sh
corolla ~/Downloads
```

To build and install for your user, run `bin/install` with a stable Rust toolchain installed. It installs the executable, icon and launcher entry. The window uses native Wayland on Hyprland. The folder picker uses the desktop portal.

For a pacman-managed installation, `bin/package` creates an Arch package in `packaging/`. Install that file with `sudo pacman -U`. Use `cargo test --release --locked` to run the tests.

Corolla keeps completed scans so you can reopen disks, folders, and their scanned subfolders, even after quitting. The map shows the last scan. Press **R** or click **Rescan** to pick up changes made elsewhere. You can cancel a rescan and keep exploring the previous map. Refreshing a subfolder updates its branch in cached parent maps. Recent maps are saved privately in `$XDG_CACHE_HOME/corolla/scans` (normally `~/.cache/corolla/scans`). The disk cache holds up to 12 maps within 512 MiB. Very large maps that exceed the snapshot limits remain available in memory during the session. The command-line JSON report always scans the filesystem.

## Explore and clean up

Gray petals represent files. Colored petals represent folders. Very small neighbors share a dimmed segment that opens a list. On a whole disk, purple shows used space outside the scan and the gap shows free space. The inspector ranks items by allocated size. Sparse files use their allocated blocks, and hard links share one allocation. Btrfs reflinks and snapshots can share physical extents, so removing an item may release less space than its displayed size.

Drag a petal or inspector row to the collector. Open the collector to review or remove items, move them to the desktop Trash, or permanently delete them after confirmation. Empty the desktop Trash to reclaim space from trashed files. Cleanup runs with your normal user permissions.

| Shortcut | Action |
| --- | --- |
| Ctrl+O / Ctrl+L | Choose a folder / enter its path |
| Ctrl+D | Pin or unpin the current folder in the overview |
| ↑ / ↓ | Select an item |
| Enter / → | Open the selection |
| Space | Preview the item, or scan the first disk from the overview |
| ← / Esc | Go to the parent, dismiss a dialog, or cancel a scan |
| Alt+← / Alt+→ | Go back / forward |
| Delete / C | Collect the highlighted item / review the collector |
| R | Rescan or refresh the volume list |
| ? / Q | Show shortcuts / quit |

For a disk-usage report without opening a window, run `corolla scan --json DIRECTORY`. It prints the same allocated-byte totals used by the map, immediate children in size order, and the count of unreadable entries.

## License

Corolla is distributed under the [MIT License](LICENSE). The bundled Inter font is distributed under the [SIL Open Font License 1.1](assets/Inter-LICENSE.txt).
