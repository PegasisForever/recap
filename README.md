# Recap Flatpak remote

This branch is a static OSTree repository served by GitHub Pages. It holds no
source code. Regenerate it with `packaging/publish.sh` on the `master` branch.

```bash
flatpak remote-add --user --if-not-exists recap https://pegasisforever.github.io/recap/recap.flatpakrepo
flatpak install --user recap site.pegasis.Recap
```
