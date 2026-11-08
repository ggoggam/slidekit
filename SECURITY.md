# Security

slidekit parses whole-slide images from wherever its caller points it, including files from
untrusted sources and remote object stores. Please report anything a crafted file could cause
privately, through [GitHub's private vulnerability reporting](https://github.com/ggoggam/slidekit/security/advisories/new),
not in a public issue. That includes memory-safety bugs (in the vendored OpenJPEG too), panics,
unbounded allocations, hangs, or reads outside the slide's own files.

Include the format, a description of what triggers it and the slidekit version or commit. A
minimal synthetic file is the best reproduction. Never send a clinical slide or anything with
patient data in it.

Vulnerabilities in OpenSlide itself go to [the OpenSlide project](https://github.com/openslide/openslide/security).
