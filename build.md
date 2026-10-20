Building from Source
--------------------------

Building rquickshare from source requires conda for the toolchain and GTK stack, plus 4 system packages for libraries not available on conda-forge.

#### Prerequisites

- [Miniconda](https://docs.conda.io/en/latest/miniconda.html) (or full Anaconda)
- `git`
- An Ubuntu 26.04 LTS system (other distros may work with minor adjustments)

#### Step 1: Install system dependencies

The following packages are not available on conda-forge and must be installed via `apt`:

```bash
sudo apt update
sudo apt install \
  libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev
```

> **Note:** `libwebkit2gtk-4.1-dev` provides the Tauri webview. `libayatana-appindicator3-dev` provides system tray support. `librsvg2-dev` is needed for the AppImage bundler.

#### Step 2: Clone and setup the conda environment

```bash
git clone https://github.com/asulwer/rquickshare.git
cd rquickshare
conda env create -f environment.yml
conda activate rquickshare
```

#### Step 3: Configure environment variables

Add the following to your `~/.bashrc` (or `~/.zshrc`, etc.):

> **Note:** These instructions assume conda is installed at `/home/$USER/miniconda3`. Adjust the `PKG_CONFIG_PATH` and `PKG_CONFIG_LIBDIR` values if your conda installation is located elsewhere (e.g., `~/.conda/envs/` or `/opt/conda/envs/`).

```bash
# PKG_CONFIG: find .pc files from both conda and system
export PKG_CONFIG_PATH="/home/$USER/miniconda3/envs/rquickshare/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig"
export PKG_CONFIG_LIBDIR="/home/$USER/miniconda3/envs/rquickshare/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig"

# RUSTFLAGS: tell conda's cross-linker where to find system .so files
export RUSTFLAGS="-L /usr/lib/x86_64-linux-gnu"
```

Then reload:

```bash
source ~/.bashrc
```

#### Step 4: Build

**core_lib (Rust library):**

```bash
cd core_lib
cargo build --release
```

**app/main (Tauri desktop app):**

```bash
cd app/main
pnpm install tauri
pnpm build
```

This produces three bundles:

- `.deb` — for Debian/Ubuntu-based distributions
- `.rpm` — for Fedora/RHEL-based distributions
- `.AppImage` — portable, no installation required

#### Quick build (one-liner)

```bash
conda activate rquickshare && \
export PKG_CONFIG_PATH="/home/$USER/miniconda3/envs/rquickshare/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export PKG_CONFIG_LIBDIR="/home/$USER/miniconda3/envs/rquickshare/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export RUSTFLAGS="-L /usr/lib/x86_64-linux-gnu" && \
pnpm install tauri && cd app/main && pnpm build
```
