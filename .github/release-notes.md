## Desktop installation notes

Morrow desktop is currently distributed without a commercial Windows signing certificate or Apple Developer notarization. Download installers only from this project’s GitHub Release page.

### Windows

Download `Morrow_<version>_x64-setup.exe`. If SmartScreen appears, select **More info**, verify that the file came from this GitHub Release, then choose **Run anyway**.

### macOS

Choose the `aarch64` DMG for Apple Silicon or the `x64` DMG for an Intel Mac, then drag Morrow to Applications. On first launch, open Finder and right-click **Morrow → Open**. If macOS still blocks it, use **System Settings → Privacy & Security → Open Anyway**.

Desktop updates are manual and Morrow does not perform background update checks: download the newer installer and install it over the existing copy. Configuration, MCP settings, commands, and sessions remain under `~/.morrow` and are not removed by an app upgrade. Downgrades also require manually installing an older GitHub Release.
