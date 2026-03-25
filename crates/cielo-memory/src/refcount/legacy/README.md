# Legacy Core-level ARC prototype

These files were present but not linked by the original compiler module tree.
They are kept beside the active CFG-level reference-counting implementation so
their ideas and fixtures are not lost during the crate split.  Do not add them
to the public pass pipeline unchanged: first replace their missing legacy rule
configuration with a `RefcountProfile`, then make their tests active.
