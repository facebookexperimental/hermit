These two schema-10 fixtures are synthetic production-parser controls retained by
`ledger::schema10::tests::exact_artifacts_derive_compact_summaries_and_refuse_independent_mutations`
at Hermit edd48bf2040e7faa9330a8262e4af03555f30ab7. They are not guest measurements.
The validation writer tests consume these same complete operand reports and plan
bytes instead of maintaining another comparison-report fixture definition.

The current synthetic cell fixture adds the required `--strict` argument to
both operand invocations. Its embedded verification reports and the plan bytes
remain those of the original fixture. Current parser checks export a fresh
complete set; the original retained files remain historical evidence.
