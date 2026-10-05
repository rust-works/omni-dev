# Non-Rust coverage reports

Small producer reports used by `tests/coverage_diff_test.rs`. The tests create
real git revisions and run `coverage diff` with explicit mappings, checking both
covered and uncovered executable lines. Summary fields are not used as line
counts. All reports retain their producer's filename spelling.

## Locally generated

Generated on 2026-10-05 in `/private/tmp/2188-producers/`; no timestamp or path
rewrites were applied. These temporary paths deliberately represent another
runner, so tests cannot accidentally rely on the local checkout root.

- `nyc.lcov`: nyc 18.0.0, run from `packages/web` with
  `nyc --reporter=lcov node src/calc.js`. The source was:

  ```js
  function choose(flag) {
    if (flag) return 1;
    return 0;
  }
  choose(true);
  ```

  Three executable lines, two covered. `SF:src/calc.js` needs the package root.
- `coverage-py.xml`: coverage.py 7.16.2, from a `python` directory, using
  `coverage run --source=src src/calc.py` then `coverage xml -o coverage.xml`.
  The source was:

  ```python
  def choose(flag):
      if flag:
          return 1
      return 0

  choose(True)
  ```

  Five executable lines, four covered. The class filename is `src/calc.py`;
  `<sources>` records the producer directory but is not used for attribution.
- `dart.lcov`: Dart SDK 3.13.5, package:coverage 1.15.1, package:test 1.32.0.
  Run `dart run coverage:test_with_coverage` in a package named `fixture_app`
  with these dev dependencies and SDK constraint `^3.0.0`. `lib/calc.dart` was:

  ```dart
  int covered() => 1;
  int uncovered() => 0;
  ```

  The test imported `package:test/test.dart` and `package:fixture_app/calc.dart`
  and ran `test('covered function', () => expect(covered(), 1))`. Two executable
  lines, one covered. The absolute `SF:` path needs the original runner root
  replaced with the monorepo package directory.

## Upstream captures

These reports were not regenerated locally. Their line records and source
filenames are retained; they are data fixtures rather than copied source code.

- `gcovr.xml`: the complete [gcovr example report](https://github.com/gcovr/gcovr/blob/49fbbe53d164d8b0d9764641e7902fa1cfa6cb68/doc/examples/example_cobertura.xml),
  marked `gcovr 8.6+main`. Seven executable lines, six covered;
  `filename="example.cpp"` is source-root-relative. Upstream gcovr is BSD-3-Clause.
- `coverlet.xml`: one `<class>` from ReportGenerator's
  [coverlet Cobertura capture](https://github.com/danielpalme/ReportGenerator/blob/47616607cebb0e7fec10fd44a530c8712bcf751d/src/Testprojects/CSharp/Reports/Cobertura_coverlet.xml).
  The other classes were removed and the XML reindented; all attributes,
  methods and line records in the retained class are unchanged. Root/package
  summary counters still describe the original report, intentionally proving
  that only per-line records matter. Eight executable lines, four covered;
  `filename="AbstractClass.cs"` and `<source>C:\temp\</source>` require an
  explicit root mapping. The original XML `version="1.9"` identifies the
  report schema, not a known coverlet release. Upstream ReportGenerator is
  Apache-2.0.
