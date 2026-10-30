# Development note

## Release

Update the version numbers of the following files.

- `Cargo.toml`

Commit changes.

```
git add -A
git commit -m "Release v0.1.0"
git push origin main:main
```

By pushing a tag, the github action creates a release.

```
git tag v0.1.0
git push origin v0.1.0
```
