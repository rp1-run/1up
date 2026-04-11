# Changelog

All notable changes to `1up` are recorded in this file.

This project follows an install-first public release posture:

- `Cargo.toml` is the version source of truth
- Git tags use the form `vX.Y.Z`
- GitHub Releases and this changelog together form the public release record

## [0.1.2](https://github.com/rp1-run/1up/compare/v0.1.1...v0.1.2) (2026-04-11)


### Features

* add a basic perf benchmark ([ae9a9ec](https://github.com/rp1-run/1up/commit/ae9a9ec856e8a4beebf5803218587d2d8d4fd009))
* add a benchmark script ([e534e30](https://github.com/rp1-run/1up/commit/e534e30a4b19b58fcfe9f39f3392fd715952aae5))
* add agent skill ([72c2281](https://github.com/rp1-run/1up/commit/72c2281f86c63a1e546a4b31d6a7a5f79370ce80))
* add fenced agent reminders and daemon fixes ([#8](https://github.com/rp1-run/1up/issues/8)) ([7564209](https://github.com/rp1-run/1up/commit/7564209e025bb04ba909c9cb003e26d3c2d89b0d))
* add justfile ([0ce30d5](https://github.com/rp1-run/1up/commit/0ce30d5ffa6b60ab0309d8993b383d5b0f68a9b0))
* add Kotlin tree-sitter support and fix clippy warnings ([cb74325](https://github.com/rp1-run/1up/commit/cb74325eb3b37bd3c508731a531bbaa02c368462))
* add progress counter to scan/parse spinner ([d9543a0](https://github.com/rp1-run/1up/commit/d9543a0ea7aac2aa928a9f0e00fe09bcb60a468a))
* add tree-sitter support for CSS, HTML, JSON, Bash, TOML, YAML, and Markdown ([c0605e3](https://github.com/rp1-run/1up/commit/c0605e344407bb09b3bb9b931de494eae137f870))
* add update flow and harden release automation ([#12](https://github.com/rp1-run/1up/issues/12)) ([1084b28](https://github.com/rp1-run/1up/commit/1084b28752f0bee310b2d952b4b22751aab4b5c9))
* **cli:** release ready ([a71264f](https://github.com/rp1-run/1up/commit/a71264f90fe001a3cf88cee1ec4d1737680f4b83))
* ensure bun uses os version packages ([66a1a72](https://github.com/rp1-run/1up/commit/66a1a7279fee1dfded1f4dfb3cccf8380d35dc52))
* **evals:** add 1up tool docs to prompt, efficiency scores, eval tooling ([dad1c0d](https://github.com/rp1-run/1up/commit/dad1c0dbf6558c27b8d5ee504f4ccb9e3f43e28e))
* **evals:** add 1up vs baseline comparison with all 4 eval tasks ([e884d0f](https://github.com/rp1-run/1up/commit/e884d0fe3f951a59b9949643924103086a42920f))
* **evals:** implement T4 - extension hook for workspace isolation and fixture management ([ddb0f40](https://github.com/rp1-run/1up/commit/ddb0f406a2b4bff0ec609aa1cf398154ac1e5b30))
* **evals:** implement T5 - eval suite config and prompt template ([d3276a5](https://github.com/rp1-run/1up/commit/d3276a54835aa0ff636615a3599500e540b150df))
* **evals:** implement T6, T3 - gitignore updates and shared assertions ([3c64087](https://github.com/rp1-run/1up/commit/3c64087f523303aca63da171d41e1aaa74974a18))
* **evals:** scaffold evals package and implement tool name mapping (T1, T2) ([713692b](https://github.com/rp1-run/1up/commit/713692bba6798a830b22c6ecef277d1194cbaea7))
* harden local state, daemon IPC, and artifact verification ([9532b06](https://github.com/rp1-run/1up/commit/9532b06364219be65adb2ce0ceff1324d50ef02c))
* make the daemon live longer ([4866943](https://github.com/rp1-run/1up/commit/4866943c02b43c3f3fe98fd4f0aea42a6c4520cf))
* migrate from nanospinner/indicatif to zenity for CLI UI ([f046df4](https://github.com/rp1-run/1up/commit/f046df4190909da83ca64272f2fc7b7ffadcbbe6))
* migrate from turso to libsql for database layer ([c343e15](https://github.com/rp1-run/1up/commit/c343e1525fb01f2637a749417aadea0b338edc2a))
* nanospinner UI, suppress warnings, skip unknown file types ([0627f49](https://github.com/rp1-run/1up/commit/0627f49ee35d211e4699b3c371d4026ab391b09e))
* optimize indexing and search pipeline ([7d3f299](https://github.com/rp1-run/1up/commit/7d3f299c6f272f8658fac662891908ffcc73efb3))
* **optimize:** implement T1 - restore scoped scan parity ([7174925](https://github.com/rp1-run/1up/commit/7174925df0fb35657d481170009baf958ce64135))
* **optimize:** implement T1 - scoped run planning ([bf2d371](https://github.com/rp1-run/1up/commit/bf2d371363c246acb7bfbe538e6bf468626d4419))
* **optimize:** implement T2 - add exact-first symbol index ([ebbc04d](https://github.com/rp1-run/1up/commit/ebbc04dd5e5420916653fcb1446bd02ba6466a29))
* **optimize:** implement T3 - add warm embedding runtime ([363d68f](https://github.com/rp1-run/1up/commit/363d68fc2f83c5f6bda136cecb290d6a6df2cb7b))
* **optimize:** implement T3 - daemon-backed warm search reuse ([3547a7e](https://github.com/rp1-run/1up/commit/3547a7eaeaa5b2f45507ca247b074d92419d625c))
* **optimize:** implement T4 - tune adaptive writer batching ([4556b4d](https://github.com/rp1-run/1up/commit/4556b4d0346bff4b138d3bb7dd7fbedffe43b53c))
* **optimize:** implement T5 - add candidate-first hybrid retrieval ([dd8e6ac](https://github.com/rp1-run/1up/commit/dd8e6acd39e83463e81915d0462f6d5fa0ebbadd))
* **optimize:** implement T6 - expand validation and benchmark evidence ([ca7d298](https://github.com/rp1-run/1up/commit/ca7d298a1f82e291906881f87442ab98edc6838c))
* **parallel:** implement T1 - shared indexing config resolution ([0b2052f](https://github.com/rp1-run/1up/commit/0b2052ff49e69331a861e5179a078d057e18b55e))
* **parallel:** implement T2 - add storage metadata helpers ([316318a](https://github.com/rp1-run/1up/commit/316318a452637a44e8a0994b4a2dc6c66523b861))
* **parallel:** implement T3 - staged indexing pipeline ([d0ab552](https://github.com/rp1-run/1up/commit/d0ab5523e4d3040402aaf81e842769aff5e05d37))
* **parallel:** implement T3,T5,T6 - fix release indexing regression ([992eee3](https://github.com/rp1-run/1up/commit/992eee381b6573b7b6e68dd5fc07f0a05cc8b4f4))
* **parallel:** implement T4 - daemon burst scheduling ([480b8b5](https://github.com/rp1-run/1up/commit/480b8b538946364fca8f94cecb84bd6626320dec))
* **parallel:** implement T5 - isolate timings and benchmark runs ([61ce026](https://github.com/rp1-run/1up/commit/61ce026d605576d5e4b15f0e42e6ab2a515c2250))
* **parallel:** implement T5 - observability and benchmark support ([bf6ceaf](https://github.com/rp1-run/1up/commit/bf6ceafe9a56a9ba3a043c9b486a300db495621e))
* **parallel:** implement T6 - regression coverage and verification ([740cbfe](https://github.com/rp1-run/1up/commit/740cbfeb42e28015fbb7d3f9443ab0322d9060c0))
* **release-ready-1:** implement T1 - normalize Apache licensing ([98e3e48](https://github.com/rp1-run/1up/commit/98e3e4857b223448c59a61c5f06dc8aec3d28028))
* **release-ready-1:** implement T2 - restructure onboarding and governance ([d52d44a](https://github.com/rp1-run/1up/commit/d52d44a1fe132970613e02d486cfedb4e1f4ab37))
* **release-ready-1:** implement T3 - add Windows local-mode boundary ([df09248](https://github.com/rp1-run/1up/commit/df092483e6ee1bbfcb724477f373e1b0877d7219))
* **release-ready-1:** implement T4 - build release asset pipeline ([9b6ab9c](https://github.com/rp1-run/1up/commit/9b6ab9c8fa16b5dc28d66185f2e98e07d2d1b8b2))
* **release-ready-1:** implement T5 - automate package publishing ([a2df0a1](https://github.com/rp1-run/1up/commit/a2df0a1a45e58d6c835a3b44c6cc829b4739f90e))
* **release-ready-1:** implement T6 - add merge gates and release evidence ([dcbcc9b](https://github.com/rp1-run/1up/commit/dcbcc9bc311f73f7ae45815e7ab11b72929fda49))
* **rewrite-sql:** implement T1 - add schema v5 gating ([9e6e126](https://github.com/rp1-run/1up/commit/9e6e126cf4c8fec5779736cec51860e21163920f))
* **rewrite-sql:** implement T2 - add retrieval backends ([7098c16](https://github.com/rp1-run/1up/commit/7098c168d19b461e8a2f1537aab88516bd08ba30))
* **rewrite-sql:** implement T3 - native vector write path ([9f02189](https://github.com/rp1-run/1up/commit/9f02189addb1fcc52f5b376c87cf4ce2d272fec3))
* **rewrite-sql:** implement T4 - add verification evidence ([c86c3f1](https://github.com/rp1-run/1up/commit/c86c3f11a6da96a77fafff4207b4cf52d11975cb))
* **rewrite-sql:** implement T5 - capture adoption guidance ([e082d7e](https://github.com/rp1-run/1up/commit/e082d7e50a3e2f7ff51a8f5c6299b7d37f035649))
* **rewrite-sql:** implement TX-20260402193656 - fix verify blockers ([756ddbd](https://github.com/rp1-run/1up/commit/756ddbd5ffe5a1cd8e9c4578d0790c65602ecd40))
* **rewrite-sql:** implement TX-20260402200524 - refresh rerun-verify evidence ([3c82412](https://github.com/rp1-run/1up/commit/3c82412e50f6717cf7af795691ea0e6605f4c11e))
* **security-fixes:** implement T1 - add secure filesystem primitives ([4856eb6](https://github.com/rp1-run/1up/commit/4856eb6f3b31c754067aaff38ba73ae265528be3))
* **security-fixes:** implement T1 - reject symlinked parent components ([ffe2434](https://github.com/rp1-run/1up/commit/ffe24341cf01b520dfaff7a3ba028b4bd625e444))
* **security-fixes:** implement T2 - harden daemon IPC ([073583f](https://github.com/rp1-run/1up/commit/073583fec16897765482488814b2c0a28944fbab))
* **security-fixes:** implement T3 - harden local state lifecycle ([213f7f6](https://github.com/rp1-run/1up/commit/213f7f6fc7df4e246c8843ff54e16b627523bed5))
* **security-fixes:** implement T4 - constrain context access ([8db2afe](https://github.com/rp1-run/1up/commit/8db2afebbf2245c4caecf2d9d3390bf18c4fe008))
* **security-fixes:** implement T4 - reject default absolute context paths ([0423384](https://github.com/rp1-run/1up/commit/042338474ea8b73572b09be1a994780cb41a4181))
* **security-fixes:** implement T5 - stabilize legacy import regression test ([9089d3f](https://github.com/rp1-run/1up/commit/9089d3f03724dfd92e2448a1454ecf9b19510897))
* **security-fixes:** implement T5 - verify model artifacts ([a825ab7](https://github.com/rp1-run/1up/commit/a825ab784a9a67c29831a3fac301fc1f59bc11dc))
* **security-fixes:** implement T6 - add release security gate ([a18aa4a](https://github.com/rp1-run/1up/commit/a18aa4a0aad765c1897ba47cd7249f852ed7eb89))
* **security-fixes:** implement T6 - fail fast security gate ([1fe6765](https://github.com/rp1-run/1up/commit/1fe6765ee9f4b834954f3accf754ea961bf6e15b))
* **security-fixes:** implement T7 - expand security regression coverage ([2357d6d](https://github.com/rp1-run/1up/commit/2357d6df32588b7d63c495b9fa78a8bb40119c03))
* **v1:** implement T1 - Cargo project scaffolding with module structure and shared types ([5850915](https://github.com/rp1-run/1up/commit/5850915f40eaab5aac5e8fa87013742fc8fe3d5e))
* **v1:** implement T1 - remove stub file comments ([e2c5070](https://github.com/rp1-run/1up/commit/e2c5070f975c8f78473aa5d3509381215e088cbd))
* **v1:** implement T10 - scope-aware context retrieval ([b8e2792](https://github.com/rp1-run/1up/commit/b8e27922686886358ad27e01e025d42985813169))
* **v1:** implement T11 - daemon worker with file watching and multi-project support ([66f4aa8](https://github.com/rp1-run/1up/commit/66f4aa8d506816db7a612a2ed723c9d0339e01e2))
* **v1:** implement T12 - init/start/stop/status commands with auto-start ([89ace16](https://github.com/rp1-run/1up/commit/89ace16b3af96af9cf0d35cc9276f0fe34e40be0))
* **v1:** implement T13 - graceful degradation with FTS-only fallback ([73a24ce](https://github.com/rp1-run/1up/commit/73a24ce6573ab62ec8f283e99439d665107aa3d4))
* **v1:** implement T14 - integration test suite and benchmarks ([9bd8201](https://github.com/rp1-run/1up/commit/9bd820190016bfc57b9a4675aa24f675b4d5653a))
* **v1:** implement T2 - storage layer with libSQL, schema DDL, segment CRUD, and meta KV ([3f5fa6e](https://github.com/rp1-run/1up/commit/3f5fa6eab0949f0f06c264d28bd0102d0284e012))
* **v1:** implement T3 - CLI skeleton with clap derive commands, output formatters, and tracing ([78e2152](https://github.com/rp1-run/1up/commit/78e2152831bd3f9241e81e193f6d5db4cf952cea))
* **v1:** implement T4 - tree-sitter parser with 8-language grammar integration ([7145ec1](https://github.com/rp1-run/1up/commit/7145ec1884e3f9e3c03dd8cfd8b9ab0dcb6b2fe2))
* **v1:** implement T5 - sliding-window text chunker ([7c776c8](https://github.com/rp1-run/1up/commit/7c776c8d5c85e3f06200512ccc8b476df9675ea8))
* **v1:** implement T6 - embedding engine with ONNX inference ([c673333](https://github.com/rp1-run/1up/commit/c6733336a844d42a711841aa8a04841afc13b6df))
* **v1:** implement T7 - indexing pipeline with file scanner and incremental detection ([220aee2](https://github.com/rp1-run/1up/commit/220aee228032d3dcf0b9b8c3ca97033b00292983))
* **v1:** implement T8 - hybrid search engine with RRF fusion and intent detection ([2f1fc89](https://github.com/rp1-run/1up/commit/2f1fc893b8521eef1b1d21d621c2023c9d7277a7))
* **v1:** implement T9 - symbol lookup and reference search ([3ea3124](https://github.com/rp1-run/1up/commit/3ea31247b7f23235ced6c19318b1310eb9db9ec3))
* **v1:** implement TD1, TD2 - add README and KB documentation ([0a5cefe](https://github.com/rp1-run/1up/commit/0a5cefe975e6a2e489a79a80be847e9205860534))
* **v1:** implement TX-fix-clippy - fix all clippy warnings for zero-warning builds ([6efa5d7](https://github.com/rp1-run/1up/commit/6efa5d72370b504ef773bc11ecfb63ee4d0b66d7))
* **v1:** implement TX-fix-clippy-and-int8 - fix clippy tautology and int8 vector search ([ee8e261](https://github.com/rp1-run/1up/commit/ee8e26147cfe853f11ae583cc25f7e174e866a4d))
* **v1:** implement TX-migrate-to-turso - migrate from libsql to turso crate ([646ecde](https://github.com/rp1-run/1up/commit/646ecde4317f568d7f27b952e078efe88291fd90))
* **v1:** implement TX-structural-search - AST-pattern structural search ([f50db19](https://github.com/rp1-run/1up/commit/f50db19bd028c5372bf7d9e8f717ed7bb3f8ca34))
* **v1:** implement TX-update-deps - update all crate dependencies to latest versions ([41867b4](https://github.com/rp1-run/1up/commit/41867b4053b53bf430d9e706fc1f908bd0f1688a))


### Bug Fixes

* auto-recover from corrupt database schema on migrate ([e55f9f7](https://github.com/rp1-run/1up/commit/e55f9f7152b0755056735287bda97a95e24f40d1))
* **bench:** canonicalize temp roots for hardened paths ([173f8d3](https://github.com/rp1-run/1up/commit/173f8d31e1c3dffacfdd5a4d50b651422203bc89))
* default output format to human instead of json ([452bef2](https://github.com/rp1-run/1up/commit/452bef23d550df576936ba940e9de42a348407f2))
* enable spinner animation on stderr with TTY detection ([8cb0457](https://github.com/rp1-run/1up/commit/8cb04574f1a8f431388ae592e66023df825b14c9))
* eval lookups ([1e4dd91](https://github.com/rp1-run/1up/commit/1e4dd918a2c5ac06808af0b86db86f930b6f3876))
* **evals:** fix ESM import resolution and shallow clone for eval runtime ([df025be](https://github.com/rp1-run/1up/commit/df025becde4f62e3666b3f1af63bfceda45a4f20))
* **evals:** small fixes ([c945df7](https://github.com/rp1-run/1up/commit/c945df7ae27ada149c05949328b752705bd59708))
* **evals:** use provider metadata for deterministic tool-call assertions ([3d070fd](https://github.com/rp1-run/1up/commit/3d070fdb43d67cfb5e04c572c026eb0dbc4ed320))
* **feedback:** guard embedding model metadata against legacy and unbound indexes ([df3fb26](https://github.com/rp1-run/1up/commit/df3fb26da1ff7182dbc0360ae239db5c46ea1962))
* **release:** repair post-publish workflow chain ([#14](https://github.com/rp1-run/1up/issues/14)) ([a22e60f](https://github.com/rp1-run/1up/commit/a22e60f400c38ff2e5816f500450353c1103bc7c))
* retry longer for read-only DB access, improve lock error message ([524bdbc](https://github.com/rp1-run/1up/commit/524bdbcb72ce6962c2d3ec35b70cf5e793bdd39d))
* revert zenity back to nanospinner, fix progress tracking ([aec41fe](https://github.com/rp1-run/1up/commit/aec41fe5ed5d56fa77eab24d18ad38a2d03db751))
* show spinner immediately after model load during scan phase ([ecf398c](https://github.com/rp1-run/1up/commit/ecf398ce64c8bff5e4c9e15265afee1a1a8622ed))
* stabilize daemon search integration test ([#11](https://github.com/rp1-run/1up/issues/11)) ([aca685f](https://github.com/rp1-run/1up/commit/aca685fda7beb53951542206f8a09e3005e17d6b))
* use turso vector JSON array format for storage and queries ([5b69b4e](https://github.com/rp1-run/1up/commit/5b69b4e1af2cdd72f0362cf16162ee34324d91c7))


### Performance Improvements

* patch turso async_io, drop FTS during bulk insert, expand binary skip list ([e671283](https://github.com/rp1-run/1up/commit/e671283539e2a2a49841d96079e862cc0ee81353))
* use FTS prefilter for vector search instead of full table scan ([53ee24c](https://github.com/rp1-run/1up/commit/53ee24c0cf6e5c2cdfe949718bf7050a2789e590))


### Documentation

* add logo ([ab4f405](https://github.com/rp1-run/1up/commit/ab4f405fd17383a9132997b0ffdfaf7b3cb844f5))
* readme and development.md ([f24f6f5](https://github.com/rp1-run/1up/commit/f24f6f56fabec3ecc8ca14160a97b0c1a530e574))
* **security:** update hardening documentation ([c81484f](https://github.com/rp1-run/1up/commit/c81484f1490f7fa2b8e4f5d965c2894bf3eb3e2a))
* sync parallel indexing docs ([3acaa19](https://github.com/rp1-run/1up/commit/3acaa19564f0b1a486ad2729c16794aa6885ba8d))
* sync rewrite-sql documentation ([b102954](https://github.com/rp1-run/1up/commit/b1029544a0a78536299855318930fc4c179a5231))

## [0.1.1](https://github.com/rp1-run/1up/compare/oneup-v0.1.0...oneup-v0.1.1) (2026-04-11)


### Features

* add a basic perf benchmark ([ae9a9ec](https://github.com/rp1-run/1up/commit/ae9a9ec856e8a4beebf5803218587d2d8d4fd009))
* add a benchmark script ([e534e30](https://github.com/rp1-run/1up/commit/e534e30a4b19b58fcfe9f39f3392fd715952aae5))
* add agent skill ([72c2281](https://github.com/rp1-run/1up/commit/72c2281f86c63a1e546a4b31d6a7a5f79370ce80))
* add fenced agent reminders and daemon fixes ([#8](https://github.com/rp1-run/1up/issues/8)) ([7564209](https://github.com/rp1-run/1up/commit/7564209e025bb04ba909c9cb003e26d3c2d89b0d))
* add justfile ([0ce30d5](https://github.com/rp1-run/1up/commit/0ce30d5ffa6b60ab0309d8993b383d5b0f68a9b0))
* add Kotlin tree-sitter support and fix clippy warnings ([cb74325](https://github.com/rp1-run/1up/commit/cb74325eb3b37bd3c508731a531bbaa02c368462))
* add progress counter to scan/parse spinner ([d9543a0](https://github.com/rp1-run/1up/commit/d9543a0ea7aac2aa928a9f0e00fe09bcb60a468a))
* add tree-sitter support for CSS, HTML, JSON, Bash, TOML, YAML, and Markdown ([c0605e3](https://github.com/rp1-run/1up/commit/c0605e344407bb09b3bb9b931de494eae137f870))
* add update flow and harden release automation ([#12](https://github.com/rp1-run/1up/issues/12)) ([1084b28](https://github.com/rp1-run/1up/commit/1084b28752f0bee310b2d952b4b22751aab4b5c9))
* **cli:** release ready ([a71264f](https://github.com/rp1-run/1up/commit/a71264f90fe001a3cf88cee1ec4d1737680f4b83))
* ensure bun uses os version packages ([66a1a72](https://github.com/rp1-run/1up/commit/66a1a7279fee1dfded1f4dfb3cccf8380d35dc52))
* **evals:** add 1up tool docs to prompt, efficiency scores, eval tooling ([dad1c0d](https://github.com/rp1-run/1up/commit/dad1c0dbf6558c27b8d5ee504f4ccb9e3f43e28e))
* **evals:** add 1up vs baseline comparison with all 4 eval tasks ([e884d0f](https://github.com/rp1-run/1up/commit/e884d0fe3f951a59b9949643924103086a42920f))
* **evals:** implement T4 - extension hook for workspace isolation and fixture management ([ddb0f40](https://github.com/rp1-run/1up/commit/ddb0f406a2b4bff0ec609aa1cf398154ac1e5b30))
* **evals:** implement T5 - eval suite config and prompt template ([d3276a5](https://github.com/rp1-run/1up/commit/d3276a54835aa0ff636615a3599500e540b150df))
* **evals:** implement T6, T3 - gitignore updates and shared assertions ([3c64087](https://github.com/rp1-run/1up/commit/3c64087f523303aca63da171d41e1aaa74974a18))
* **evals:** scaffold evals package and implement tool name mapping (T1, T2) ([713692b](https://github.com/rp1-run/1up/commit/713692bba6798a830b22c6ecef277d1194cbaea7))
* harden local state, daemon IPC, and artifact verification ([9532b06](https://github.com/rp1-run/1up/commit/9532b06364219be65adb2ce0ceff1324d50ef02c))
* make the daemon live longer ([4866943](https://github.com/rp1-run/1up/commit/4866943c02b43c3f3fe98fd4f0aea42a6c4520cf))
* migrate from nanospinner/indicatif to zenity for CLI UI ([f046df4](https://github.com/rp1-run/1up/commit/f046df4190909da83ca64272f2fc7b7ffadcbbe6))
* migrate from turso to libsql for database layer ([c343e15](https://github.com/rp1-run/1up/commit/c343e1525fb01f2637a749417aadea0b338edc2a))
* nanospinner UI, suppress warnings, skip unknown file types ([0627f49](https://github.com/rp1-run/1up/commit/0627f49ee35d211e4699b3c371d4026ab391b09e))
* optimize indexing and search pipeline ([7d3f299](https://github.com/rp1-run/1up/commit/7d3f299c6f272f8658fac662891908ffcc73efb3))
* **optimize:** implement T1 - restore scoped scan parity ([7174925](https://github.com/rp1-run/1up/commit/7174925df0fb35657d481170009baf958ce64135))
* **optimize:** implement T1 - scoped run planning ([bf2d371](https://github.com/rp1-run/1up/commit/bf2d371363c246acb7bfbe538e6bf468626d4419))
* **optimize:** implement T2 - add exact-first symbol index ([ebbc04d](https://github.com/rp1-run/1up/commit/ebbc04dd5e5420916653fcb1446bd02ba6466a29))
* **optimize:** implement T3 - add warm embedding runtime ([363d68f](https://github.com/rp1-run/1up/commit/363d68fc2f83c5f6bda136cecb290d6a6df2cb7b))
* **optimize:** implement T3 - daemon-backed warm search reuse ([3547a7e](https://github.com/rp1-run/1up/commit/3547a7eaeaa5b2f45507ca247b074d92419d625c))
* **optimize:** implement T4 - tune adaptive writer batching ([4556b4d](https://github.com/rp1-run/1up/commit/4556b4d0346bff4b138d3bb7dd7fbedffe43b53c))
* **optimize:** implement T5 - add candidate-first hybrid retrieval ([dd8e6ac](https://github.com/rp1-run/1up/commit/dd8e6acd39e83463e81915d0462f6d5fa0ebbadd))
* **optimize:** implement T6 - expand validation and benchmark evidence ([ca7d298](https://github.com/rp1-run/1up/commit/ca7d298a1f82e291906881f87442ab98edc6838c))
* **parallel:** implement T1 - shared indexing config resolution ([0b2052f](https://github.com/rp1-run/1up/commit/0b2052ff49e69331a861e5179a078d057e18b55e))
* **parallel:** implement T2 - add storage metadata helpers ([316318a](https://github.com/rp1-run/1up/commit/316318a452637a44e8a0994b4a2dc6c66523b861))
* **parallel:** implement T3 - staged indexing pipeline ([d0ab552](https://github.com/rp1-run/1up/commit/d0ab5523e4d3040402aaf81e842769aff5e05d37))
* **parallel:** implement T3,T5,T6 - fix release indexing regression ([992eee3](https://github.com/rp1-run/1up/commit/992eee381b6573b7b6e68dd5fc07f0a05cc8b4f4))
* **parallel:** implement T4 - daemon burst scheduling ([480b8b5](https://github.com/rp1-run/1up/commit/480b8b538946364fca8f94cecb84bd6626320dec))
* **parallel:** implement T5 - isolate timings and benchmark runs ([61ce026](https://github.com/rp1-run/1up/commit/61ce026d605576d5e4b15f0e42e6ab2a515c2250))
* **parallel:** implement T5 - observability and benchmark support ([bf6ceaf](https://github.com/rp1-run/1up/commit/bf6ceafe9a56a9ba3a043c9b486a300db495621e))
* **parallel:** implement T6 - regression coverage and verification ([740cbfe](https://github.com/rp1-run/1up/commit/740cbfeb42e28015fbb7d3f9443ab0322d9060c0))
* **release-ready-1:** implement T1 - normalize Apache licensing ([98e3e48](https://github.com/rp1-run/1up/commit/98e3e4857b223448c59a61c5f06dc8aec3d28028))
* **release-ready-1:** implement T2 - restructure onboarding and governance ([d52d44a](https://github.com/rp1-run/1up/commit/d52d44a1fe132970613e02d486cfedb4e1f4ab37))
* **release-ready-1:** implement T3 - add Windows local-mode boundary ([df09248](https://github.com/rp1-run/1up/commit/df092483e6ee1bbfcb724477f373e1b0877d7219))
* **release-ready-1:** implement T4 - build release asset pipeline ([9b6ab9c](https://github.com/rp1-run/1up/commit/9b6ab9c8fa16b5dc28d66185f2e98e07d2d1b8b2))
* **release-ready-1:** implement T5 - automate package publishing ([a2df0a1](https://github.com/rp1-run/1up/commit/a2df0a1a45e58d6c835a3b44c6cc829b4739f90e))
* **release-ready-1:** implement T6 - add merge gates and release evidence ([dcbcc9b](https://github.com/rp1-run/1up/commit/dcbcc9bc311f73f7ae45815e7ab11b72929fda49))
* **rewrite-sql:** implement T1 - add schema v5 gating ([9e6e126](https://github.com/rp1-run/1up/commit/9e6e126cf4c8fec5779736cec51860e21163920f))
* **rewrite-sql:** implement T2 - add retrieval backends ([7098c16](https://github.com/rp1-run/1up/commit/7098c168d19b461e8a2f1537aab88516bd08ba30))
* **rewrite-sql:** implement T3 - native vector write path ([9f02189](https://github.com/rp1-run/1up/commit/9f02189addb1fcc52f5b376c87cf4ce2d272fec3))
* **rewrite-sql:** implement T4 - add verification evidence ([c86c3f1](https://github.com/rp1-run/1up/commit/c86c3f11a6da96a77fafff4207b4cf52d11975cb))
* **rewrite-sql:** implement T5 - capture adoption guidance ([e082d7e](https://github.com/rp1-run/1up/commit/e082d7e50a3e2f7ff51a8f5c6299b7d37f035649))
* **rewrite-sql:** implement TX-20260402193656 - fix verify blockers ([756ddbd](https://github.com/rp1-run/1up/commit/756ddbd5ffe5a1cd8e9c4578d0790c65602ecd40))
* **rewrite-sql:** implement TX-20260402200524 - refresh rerun-verify evidence ([3c82412](https://github.com/rp1-run/1up/commit/3c82412e50f6717cf7af795691ea0e6605f4c11e))
* **security-fixes:** implement T1 - add secure filesystem primitives ([4856eb6](https://github.com/rp1-run/1up/commit/4856eb6f3b31c754067aaff38ba73ae265528be3))
* **security-fixes:** implement T1 - reject symlinked parent components ([ffe2434](https://github.com/rp1-run/1up/commit/ffe24341cf01b520dfaff7a3ba028b4bd625e444))
* **security-fixes:** implement T2 - harden daemon IPC ([073583f](https://github.com/rp1-run/1up/commit/073583fec16897765482488814b2c0a28944fbab))
* **security-fixes:** implement T3 - harden local state lifecycle ([213f7f6](https://github.com/rp1-run/1up/commit/213f7f6fc7df4e246c8843ff54e16b627523bed5))
* **security-fixes:** implement T4 - constrain context access ([8db2afe](https://github.com/rp1-run/1up/commit/8db2afebbf2245c4caecf2d9d3390bf18c4fe008))
* **security-fixes:** implement T4 - reject default absolute context paths ([0423384](https://github.com/rp1-run/1up/commit/042338474ea8b73572b09be1a994780cb41a4181))
* **security-fixes:** implement T5 - stabilize legacy import regression test ([9089d3f](https://github.com/rp1-run/1up/commit/9089d3f03724dfd92e2448a1454ecf9b19510897))
* **security-fixes:** implement T5 - verify model artifacts ([a825ab7](https://github.com/rp1-run/1up/commit/a825ab784a9a67c29831a3fac301fc1f59bc11dc))
* **security-fixes:** implement T6 - add release security gate ([a18aa4a](https://github.com/rp1-run/1up/commit/a18aa4a0aad765c1897ba47cd7249f852ed7eb89))
* **security-fixes:** implement T6 - fail fast security gate ([1fe6765](https://github.com/rp1-run/1up/commit/1fe6765ee9f4b834954f3accf754ea961bf6e15b))
* **security-fixes:** implement T7 - expand security regression coverage ([2357d6d](https://github.com/rp1-run/1up/commit/2357d6df32588b7d63c495b9fa78a8bb40119c03))
* **v1:** implement T1 - Cargo project scaffolding with module structure and shared types ([5850915](https://github.com/rp1-run/1up/commit/5850915f40eaab5aac5e8fa87013742fc8fe3d5e))
* **v1:** implement T1 - remove stub file comments ([e2c5070](https://github.com/rp1-run/1up/commit/e2c5070f975c8f78473aa5d3509381215e088cbd))
* **v1:** implement T10 - scope-aware context retrieval ([b8e2792](https://github.com/rp1-run/1up/commit/b8e27922686886358ad27e01e025d42985813169))
* **v1:** implement T11 - daemon worker with file watching and multi-project support ([66f4aa8](https://github.com/rp1-run/1up/commit/66f4aa8d506816db7a612a2ed723c9d0339e01e2))
* **v1:** implement T12 - init/start/stop/status commands with auto-start ([89ace16](https://github.com/rp1-run/1up/commit/89ace16b3af96af9cf0d35cc9276f0fe34e40be0))
* **v1:** implement T13 - graceful degradation with FTS-only fallback ([73a24ce](https://github.com/rp1-run/1up/commit/73a24ce6573ab62ec8f283e99439d665107aa3d4))
* **v1:** implement T14 - integration test suite and benchmarks ([9bd8201](https://github.com/rp1-run/1up/commit/9bd820190016bfc57b9a4675aa24f675b4d5653a))
* **v1:** implement T2 - storage layer with libSQL, schema DDL, segment CRUD, and meta KV ([3f5fa6e](https://github.com/rp1-run/1up/commit/3f5fa6eab0949f0f06c264d28bd0102d0284e012))
* **v1:** implement T3 - CLI skeleton with clap derive commands, output formatters, and tracing ([78e2152](https://github.com/rp1-run/1up/commit/78e2152831bd3f9241e81e193f6d5db4cf952cea))
* **v1:** implement T4 - tree-sitter parser with 8-language grammar integration ([7145ec1](https://github.com/rp1-run/1up/commit/7145ec1884e3f9e3c03dd8cfd8b9ab0dcb6b2fe2))
* **v1:** implement T5 - sliding-window text chunker ([7c776c8](https://github.com/rp1-run/1up/commit/7c776c8d5c85e3f06200512ccc8b476df9675ea8))
* **v1:** implement T6 - embedding engine with ONNX inference ([c673333](https://github.com/rp1-run/1up/commit/c6733336a844d42a711841aa8a04841afc13b6df))
* **v1:** implement T7 - indexing pipeline with file scanner and incremental detection ([220aee2](https://github.com/rp1-run/1up/commit/220aee228032d3dcf0b9b8c3ca97033b00292983))
* **v1:** implement T8 - hybrid search engine with RRF fusion and intent detection ([2f1fc89](https://github.com/rp1-run/1up/commit/2f1fc893b8521eef1b1d21d621c2023c9d7277a7))
* **v1:** implement T9 - symbol lookup and reference search ([3ea3124](https://github.com/rp1-run/1up/commit/3ea31247b7f23235ced6c19318b1310eb9db9ec3))
* **v1:** implement TD1, TD2 - add README and KB documentation ([0a5cefe](https://github.com/rp1-run/1up/commit/0a5cefe975e6a2e489a79a80be847e9205860534))
* **v1:** implement TX-fix-clippy - fix all clippy warnings for zero-warning builds ([6efa5d7](https://github.com/rp1-run/1up/commit/6efa5d72370b504ef773bc11ecfb63ee4d0b66d7))
* **v1:** implement TX-fix-clippy-and-int8 - fix clippy tautology and int8 vector search ([ee8e261](https://github.com/rp1-run/1up/commit/ee8e26147cfe853f11ae583cc25f7e174e866a4d))
* **v1:** implement TX-migrate-to-turso - migrate from libsql to turso crate ([646ecde](https://github.com/rp1-run/1up/commit/646ecde4317f568d7f27b952e078efe88291fd90))
* **v1:** implement TX-structural-search - AST-pattern structural search ([f50db19](https://github.com/rp1-run/1up/commit/f50db19bd028c5372bf7d9e8f717ed7bb3f8ca34))
* **v1:** implement TX-update-deps - update all crate dependencies to latest versions ([41867b4](https://github.com/rp1-run/1up/commit/41867b4053b53bf430d9e706fc1f908bd0f1688a))


### Bug Fixes

* auto-recover from corrupt database schema on migrate ([e55f9f7](https://github.com/rp1-run/1up/commit/e55f9f7152b0755056735287bda97a95e24f40d1))
* **bench:** canonicalize temp roots for hardened paths ([173f8d3](https://github.com/rp1-run/1up/commit/173f8d31e1c3dffacfdd5a4d50b651422203bc89))
* default output format to human instead of json ([452bef2](https://github.com/rp1-run/1up/commit/452bef23d550df576936ba940e9de42a348407f2))
* enable spinner animation on stderr with TTY detection ([8cb0457](https://github.com/rp1-run/1up/commit/8cb04574f1a8f431388ae592e66023df825b14c9))
* eval lookups ([1e4dd91](https://github.com/rp1-run/1up/commit/1e4dd918a2c5ac06808af0b86db86f930b6f3876))
* **evals:** fix ESM import resolution and shallow clone for eval runtime ([df025be](https://github.com/rp1-run/1up/commit/df025becde4f62e3666b3f1af63bfceda45a4f20))
* **evals:** small fixes ([c945df7](https://github.com/rp1-run/1up/commit/c945df7ae27ada149c05949328b752705bd59708))
* **evals:** use provider metadata for deterministic tool-call assertions ([3d070fd](https://github.com/rp1-run/1up/commit/3d070fdb43d67cfb5e04c572c026eb0dbc4ed320))
* **feedback:** guard embedding model metadata against legacy and unbound indexes ([df3fb26](https://github.com/rp1-run/1up/commit/df3fb26da1ff7182dbc0360ae239db5c46ea1962))
* retry longer for read-only DB access, improve lock error message ([524bdbc](https://github.com/rp1-run/1up/commit/524bdbcb72ce6962c2d3ec35b70cf5e793bdd39d))
* revert zenity back to nanospinner, fix progress tracking ([aec41fe](https://github.com/rp1-run/1up/commit/aec41fe5ed5d56fa77eab24d18ad38a2d03db751))
* show spinner immediately after model load during scan phase ([ecf398c](https://github.com/rp1-run/1up/commit/ecf398ce64c8bff5e4c9e15265afee1a1a8622ed))
* stabilize daemon search integration test ([#11](https://github.com/rp1-run/1up/issues/11)) ([aca685f](https://github.com/rp1-run/1up/commit/aca685fda7beb53951542206f8a09e3005e17d6b))
* use turso vector JSON array format for storage and queries ([5b69b4e](https://github.com/rp1-run/1up/commit/5b69b4e1af2cdd72f0362cf16162ee34324d91c7))


### Performance Improvements

* patch turso async_io, drop FTS during bulk insert, expand binary skip list ([e671283](https://github.com/rp1-run/1up/commit/e671283539e2a2a49841d96079e862cc0ee81353))
* use FTS prefilter for vector search instead of full table scan ([53ee24c](https://github.com/rp1-run/1up/commit/53ee24c0cf6e5c2cdfe949718bf7050a2789e590))


### Documentation

* add logo ([ab4f405](https://github.com/rp1-run/1up/commit/ab4f405fd17383a9132997b0ffdfaf7b3cb844f5))
* readme and development.md ([f24f6f5](https://github.com/rp1-run/1up/commit/f24f6f56fabec3ecc8ca14160a97b0c1a530e574))
* **security:** update hardening documentation ([c81484f](https://github.com/rp1-run/1up/commit/c81484f1490f7fa2b8e4f5d965c2894bf3eb3e2a))
* sync parallel indexing docs ([3acaa19](https://github.com/rp1-run/1up/commit/3acaa19564f0b1a486ad2729c16794aa6885ba8d))
* sync rewrite-sql documentation ([b102954](https://github.com/rp1-run/1up/commit/b1029544a0a78536299855318930fc4c179a5231))

## [Unreleased]

## [0.1.0] - 2026-04-09

### Added

- Public release automation for GitHub release assets, checksums, manifests, and publish-time evidence
- Homebrew and Scoop packaging flows driven from the release manifest
- Windows local-mode support for indexing and search when daemon workflows are unavailable
- Release operator guidance in `RELEASE.md` and contributor validation policy in `CONTRIBUTING.md`
