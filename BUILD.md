The project is divided into two parts:

- **core_lib:** This is a Rust library that encompasses all the logic necessary for discovering, connecting to, and transferring files to QuickShare-compatible clients.
- **app/main:** A Tauri application that utilizes core_lib to handle incoming requests and initiate outgoing ones.

How to build
--------------------------

### core_lib

Building the core_lib is straightforward because it is a basic Rust project.

Install `protobuf-compiler` system package, and then simply run `cargo build` or `cargo build --release` from `core_lib` folder.

### app/main

The app/main is developed as a Tauri application. For package management, pnpm is recommended (though npm and others may also work, pnpm is preferred for this project).

(all commands are run inside the `app/main` folder)

First, install the necessary dependencies:

```
pnpm install
```

- To run the debug version:

```
pnpm dev
```

- To build a release package (.deb, .rpm & .AppImage on Linux, .dmg on macOS, .msi & -setup.exe on Windows):

```
pnpm build
```

For more detailed information on building the app/main and understanding any potential limitations, it's advised to consult the [Tauri documentation](https://v2.tauri.app/start).

Alternative: building with conda (Linux)
--------------------------

Instead of installing the toolchain and GTK stack system-wide, you can get most of it from conda. You still need a few system packages for libraries that aren't available on conda-forge.

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

#### Step 2: Clone and set up the conda environment

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

From the repository root, build the app (this also builds core_lib):

```bash
cd app/main
pnpm install
pnpm build
```

This produces three bundles:

- `.deb` for Debian/Ubuntu-based distributions
- `.rpm` for Fedora/RHEL-based distributions
- `.AppImage`, portable, no installation required

To build only the Rust library, run `cargo build --release` in `core_lib` instead.

#### Quick build (one-liner)

From the repository root:

```bash
conda activate rquickshare && \
export PKG_CONFIG_PATH="/home/$USER/miniconda3/envs/rquickshare/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export PKG_CONFIG_LIBDIR="/home/$USER/miniconda3/envs/rquickshare/lib/pkgconfig:/usr/lib/x86_64-linux-gnu/pkgconfig" && \
export RUSTFLAGS="-L /usr/lib/x86_64-linux-gnu" && \
cd app/main && pnpm install && pnpm build
```
