#!/usr/bin/env bash
set -euo pipefail

# Check if a version argument is provided
if [ $# -eq 0 ]; then
    echo "Please provide a version number"
    exit 1
fi

NEW_VERSION=$1

# Update version in Cargo.toml
cargo set-version "$NEW_VERSION"

# Update version in flake.nix
# This assumes a specific structure in flake.nix. Adjust the sed command as needed.
sed -i "s/version = \".*\"/version = \"$NEW_VERSION\"/" flake.nix

# Check if the sed command made changes
if ! git diff --exit-code flake.nix; then
    echo "Updated version in flake.nix"
else
    echo "No changes needed in flake.nix"
fi

# Commit the changes
git add Cargo.lock **/Cargo.toml flake.nix
git commit -m "Bump version to $NEW_VERSION"

# Create and push a new tag
git tag -a "v$NEW_VERSION" -m "Version $NEW_VERSION"
git push && git push --tags
