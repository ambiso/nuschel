# objc2 upstream notice supplements

`provenance.json` maps the 23 selected missing-notice package versions from the
objc2 repository to their published VCS revisions and exact upstream notice
bytes. This includes `block2`, `dispatch2`, and `objc-sys`. Each copied file has
an upstream URL and SHA-256. Pinned tree copies were additionally checked against
the corresponding Git blob IDs.

Older releases include a full root `LICENSE.txt`. Newer releases contain
`LICENSE.md`, which explicitly grants MIT or includes MIT among the available
license alternatives, while linking to the standard terms. The MIT alternative
is selected for these supplements. The exact newer grants are retained together
with the complete historical upstream MIT notice, rather than substituting a
template or inventing copyright names.

The historical notice is from commit
`757cd841d0293341dbefd9b99d7548dec244ffde`, immediately before commit
`cfb199226661ec6e4939fcd7cd7531b3c5453076` replaced `LICENSE.txt` with
`LICENSE.md`. Upstream compare results confirm that this historical revision is
an ancestor of each selected newer release revision. The package records include
these immutable revisions, the transition, and the comparison URLs. The
historical notice retains Steven Sheldon's original attribution; it does not
assert that he is the sole author of newer code or generated Apple bindings.

The upstream explanatory notice's Apple SDK discussion is retained verbatim.
This supplement does not resolve or replace that discussion. Nor does it cover
all distribution obligations, native framework dependencies, or other bundled
assets outside the selected Cargo runtime inventory.
