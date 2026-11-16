# Vendored assets

## htmx-2.0.4.min.js

- Source: `dist/htmx.min.js` of the npm package `htmx.org@2.0.4` (downloaded once as build tooling).
- Size: 50917 bytes.
- SHA-256 (hex): `e209dda5c8235479f3166defc7750e1dbcd5a5c1808b7792fc2e6733768fb447`
  (constant `web::assets::HTMX_SHA256`, checked by a unit test).
- SHA-384 SRI: `sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+`
  (constant `web::assets::HTMX_SRI`, used in the `integrity` attribute of `base.html`).

Cross-checks (all agree):

1. npm registry tarball `htmx.org-2.0.4.tgz`, file `package/dist/htmx.min.js`.
2. unpkg `htmx.org@2.0.4/dist/htmx.min.js`: same SHA-256.
3. jsDelivr package metadata (`data.jsdelivr.com/v1/package/npm/htmx.org@2.0.4/flat`): hash
   `4gndpcgjVHnzFm3vx3UOHbzVpcGAi3eS/C5nM3aPtEc=` (base64 SHA-256) equals the hex value above.
4. Official htmx docs at tag `v2.0.4` (`www/content/docs.md`): `integrity="sha384-HGfzto..."` equals the SRI above.

Updating htmx means changing the file, both constants, this file and the test together.
