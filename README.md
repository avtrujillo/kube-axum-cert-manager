**THIS CRATE IS A WORK-IN-PROGRESS OPEN SOURCING OF INTERNAL TOOLS.**

Watches for usable cert-manager certificates in a kubernetes cluster and uses them to serve
an axum Router over TLS.

I have been using this in production for over a year at [TODO: insert link to Transcribbit with a quick blurb, maybe mention recently having gotten permission to open source?]

The cert-manager CRD structs were created with [Kopium](https://crates.io/crates/kopium). In order to use this crate, you'll need to [install cert-manager in your cluster](https://cert-manager.io/docs/installation/)
TODO: discuss features, including aws-lc-rs and its alternatives

TODO: add an example
TODO: describe necessary permissions