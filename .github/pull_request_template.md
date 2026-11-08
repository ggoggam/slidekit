## Summary

<!-- What changed, and why. Link the issue it closes, if any: "Closes #12". -->

-

## Area

- [ ] Vendor driver (which: )
- [ ] Codec
- [ ] Public API
- [ ] Storage / I/O
- [ ] Tests, benches or CI only
- [ ] Docs only

## Validation

<!-- The commands you ran, and the slides you tried it on. -->

- [ ] `mise run pre-commit`
- [ ] `mise run test`, with the integration slides (CI runs only `test:unit`)
- [ ] Checked against OpenSlide (`mise run bench:openslide`, slidetool or openslide-python)

```sh

```

## Checklist

- [ ] Behaviour matches OpenSlide, or the difference is deliberate and documented
- [ ] Malformed input returns an `Err`, not a panic, hang or unbounded allocation
- [ ] No clinical slides, patient data, credentials or signed URLs in code, tests, commits or screenshots
- [ ] No new C dependency or system library in a default build, or a measured reason for one
- [ ] README tables updated if format, codec or API support changed
