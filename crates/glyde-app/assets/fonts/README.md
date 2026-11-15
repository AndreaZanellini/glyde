# Embedded fonts

The interface faces of the visual identity (issue #229, `docs/design/README.md`), embedded
by `src/theme.rs`. Both are under the SIL Open Font License 1.1 (`*-OFL.txt`, shipped
next to them), which allows embedding them in an Apache-2.0 application. Neither declares
a Reserved Font Name, so the subsets keep their names.

| File | Source |
|---|---|
| `Inter-Regular.ttf`, `Inter-SemiBold.ttf` | Inter 4.1, `extras/ttf/` of `Inter-4.1.zip` from github.com/rsms/inter/releases (sha256 `9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e`) |
| `JetBrainsMono-Regular.ttf` | JetBrains Mono 2.304, `fonts/ttf/` of `JetBrainsMono-2.304.zip` from github.com/JetBrains/JetBrainsMono/releases (sha256 `6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf`) |

They are subset to Latin, Greek and the symbols Glyde's text uses (units, arrows, math),
which takes the three files from 1.1 MB to about 200 KB. egui's own fonts stay
behind them as fallbacks for anything else (emoji, CJK column names). To rebuild:

```sh
pip install fonttools
U="U+0020-007E,U+00A0-017F,U+0192,U+02C6-02DC,U+0391-03C9,U+2000-206F,U+2070-209F,U+20AC,U+2100-214F,U+2190-21FF,U+2200-22FF,U+2300-23FF,U+25A0-25FF,U+2713-2717"
pyftsubset IN.ttf --unicodes="$U" --layout-features='kern' --no-hinting \
  --desubroutinize --name-IDs='*' --output-file=OUT.ttf
```
