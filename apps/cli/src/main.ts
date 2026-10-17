#!/usr/bin/env node
// Composition root: the only place allowed to wire domains to real I/O (fs, argv, stdout).

process.stderr.write("scout: no commands implemented yet\n");
process.exitCode = 1;
