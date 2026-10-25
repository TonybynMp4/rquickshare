#!/bin/bash

# Check if the base directory argument is provided
if [ -z "$1" ] || [ -z "$2" ]; then
  echo "Usage: $0 <base_directory> <build_tauri_ver> [debug_mode]"
  exit 1
fi

base_dir="$1"
tauri_ver="$2"
debug_mode="$3"

# Find all relevant files in the base directory
# .msi and .exe (the NSIS -setup.exe installer) are the Windows bundles.
files=$(find "$base_dir" -type f \( -name "*.deb" -o -name "*.rpm" -o -name "*.AppImage" -o -name "*.dmg" -o -name "*.msi" -o -name "*.exe" \))


# GLIBC only means something on Linux. Git Bash on Windows ships an ldd too,
# which would stamp a meaningless "glibc" version into the Windows filenames.
if [ "$(uname -s)" = "Linux" ] && command -v ldd &> /dev/null; then
    glib_ver=$(ldd --version | head -n1 | awk '{print $NF}')
    echo "GLIBC version: ${glib_ver}"
fi

# Loop through each file
for file in $files; do
    # Extract the directory, filename and extension
    dir=$(dirname "$file")
    filename=$(basename "$file")
    extension="${filename##*.}"

    # Extract the version and anything part. A release's version carries its
    # build number after a + (1.26.298+42), as Cargo only takes three numbers;
    # the file name gets the four-number form, 1.26.298.42.
    ver_re='([0-9]+\.[0-9]+\.[0-9]+(\+[0-9]+)?)'
    if [[ "$filename" =~ ([Rr]-?[Qq]uick[Ss]hare)_?${ver_re}_(.*)\.${extension} ]]; then
        version="${BASH_REMATCH[2]}"
        anything="${BASH_REMATCH[4]}"
    elif [[ "$filename" =~ ([Rr]-?[Qq]uick[Ss]hare)-${ver_re}-([0-9]+)\.(.*)\.${extension} ]]; then
        version="${BASH_REMATCH[2]}"
        anything="${BASH_REMATCH[4]}-${BASH_REMATCH[5]}"
    elif [[ "$filename" =~ ([rR]([-_][qQ]uick[-_][sS]hare))_?${ver_re}_?(.*)\.${extension} ]]; then
        version="${BASH_REMATCH[3]}"
        anything="${BASH_REMATCH[5]}"
    else
        echo "Filename does not match the expected pattern: $filename"
        continue
    fi
    version="v${version/+/.}"

    # Construct the new filename based on the presence of glibc and debug_mode
    if [ -n "$glib_ver" ]; then
        if [ -n "$debug_mode" ]; then
            new_filename="r-quick-share-${tauri_ver}-debug_${version}_glibc-${glib_ver}_${anything}.${extension}"
        else
            new_filename="r-quick-share-${tauri_ver}_${version}_glibc-${glib_ver}_${anything}.${extension}"
        fi
    else
        if [ -n "$debug_mode" ]; then
            new_filename="r-quick-share-${tauri_ver}-debug_${version}_${anything}.${extension}"
        else
            new_filename="r-quick-share-${tauri_ver}_${version}_${anything}.${extension}"
        fi
    fi

    # Rename the file
    mv "$file" "$dir/$new_filename"

    echo "Renamed $filename to $dir/$new_filename"
done

echo "Renaming completed."