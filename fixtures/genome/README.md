# Genome fixtures (synthetic)

`ownhealth.genome-facts/v1` files for the synthetic Rivera family: Alex (30x WGS), Sam (variant-only
VCF, facts only) and Lucía (a minor). They are copies of what `backend/src/ownhealth/genome/synthetic.py`
generates, and `tests/genome/test_genome_schema.py` fails when the two drift. `files` is empty here:
the tests and the demo seed build the matching VCF, gVCF and `.tbi` files with pysam and list them.

Genotypes, scores and recommendation text are invented; the recommendation fields are stand-ins, not
CPIC quotes.
