<p align="center"><img src="assets/logo-mark.svg" width="64" alt="ForgeCode"></p>

<h1 align="center">ForgeCode</h1>

<p align="center">IDE léger en Rust (<a href="https://gpui.rs">GPUI</a>) pour C/C++, Rust, JS/TS et le web, avec GitHub Copilot et des agents IA.</p>

## Installation

```sh
cargo run --release -- ~/mon/projet     # lancer
./scripts/bundle.sh                     # créer ForgeCode.app (macOS)
```

Fonctionne sous macOS, Linux et Windows. Pré-requis : Rust stable ; sous Linux, les bibliothèques système de GPUI (`libxkbcommon`, `libwayland`/`libxcb`, `libvulkan`, `fontconfig`). Les serveurs de langage et les débogueurs (Python, JS/TS, PHP, Go) sont installés automatiquement par ForgeCode. Débogage natif (C, C++, Rust) : `lldb-dap` (Xcode sur macOS, paquet `lldb` ailleurs).

Une version web est envisagée via la cible web de GPUI ; elle n'existe pas encore.

## Documentation

- [Guide d'utilisation](docs/guide.md)
- [Créer des extensions](docs/extensions.md)

## Licence

[PolyForm Noncommercial 1.0.0](LICENSE) : vous pouvez utiliser, modifier et partager le code, **sauf à des fins commerciales**.
