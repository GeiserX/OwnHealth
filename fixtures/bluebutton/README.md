# Blue Button fixtures

The fake CMS server uses synthetic examples from two sources.

- The inpatient and pharmacy EOBs are from the [HL7 CARIN Blue Button STU 2.1 examples](https://hl7.org/fhir/us/carin-bb/STU2.1/artifacts.html#example-example-instances). The [CARIN guide repository](https://github.com/HL7/carin-bb) publishes its examples under CC0 1.0.
- The Patient, first Coverage resource and first EOB resource come from the [CMS synthetic sample archive](https://bluebutton.cms.gov/assets/files/bb_sample_data_bbuser29999.zip) for test enrollee bbuser29999. The [CMS Blue Button site repository](https://github.com/CMSgov/bluebutton-site-static) distributes the material under CC0 1.0.

The fake substitutes the Rivera demographics and binds all claims to the synthetic patient ID at run time. Tests read these files locally.
