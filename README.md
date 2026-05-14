# amo-sign

CLI tool to package, upload, and sign Firefox extensions via the
[AMO (addons.mozilla.org) API](https://mozilla.github.io/addons-server/topics/api/overview.html).

Given an unpacked extension directory, `amo-sign` will:

1. Read the addon ID and version from `manifest.json`
2. Check if the version already exists on AMO (and skip to signing if so)
3. Package the directory into a ZIP (excluding `.git`)
4. Upload and validate the ZIP via the AMO upload API
5. Create or update the addon with the new version
6. Poll until signing completes and download the signed `.xpi`

## Usage

```
amo-sign <extension-dir> [-o signed.xpi]
```

Requires `AMO_API_KEY` and `AMO_API_SECRET` environment variables
(generate these at https://addons.mozilla.org/developers/addon/api/key/).
