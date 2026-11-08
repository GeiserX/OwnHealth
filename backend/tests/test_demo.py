from decimal import Decimal

from sqlalchemy import text

from ownhealth.demo import DEMO_HOUSEHOLD, seed_demo


def test_demo_seed_matches_the_alex_fixture(make_database, settings, blobs):
    from ownhealth.db import make_engine
    from ownhealth.db.migrate import migrate

    eng = make_engine(make_database())
    try:
        migrate(eng)
        assert seed_demo(eng, settings, blobs) is True
        assert seed_demo(eng, settings, blobs) is False  # once
        with eng.connect() as c:
            vit_d = (
                c.execute(
                    text(
                        "select value_num, interpretation, status, accepted_by, document_id from observation"
                        " where src_id = 'src_a12'"
                    )
                )
                .mappings()
                .one()
            )
            assert vit_d["value_num"] == Decimal("24") and vit_d["interpretation"] == "L"
            assert c.execute(text("select value_num from observation where src_id = 'src_a07'")).scalar() == 142
            ldl = c.execute(
                text(
                    "select count(*) from observation o join profile p on p.id = o.profile_id"
                    " where p.slug = 'alex' and alias_group = 'ldl_c'"
                )
            ).scalar()
            assert ldl == 9
            pages = c.execute(
                text("select count(*) from document_page where document_id = :d"), {"d": vit_d["document_id"]}
            ).scalar()
            assert pages == 2
            open_items = c.execute(
                text(
                    "select count(*) from review_item r join household h on h.id = r.household_id"
                    " where h.name = :n and r.status = 'open'"
                ),
                {"n": DEMO_HOUSEHOLD},
            ).scalar()
            assert open_items == 7
            active = (
                c.execute(
                    text(
                        "select name from medication_statement where src_id in ('src_m02', 'src_m05')"
                        " and med_status = 'active' order by name"
                    )
                )
                .scalars()
                .all()
            )
            assert active == ["Atorvastatin", "Omeprazole"]
            # Alex's five base metrics; lane C's Apple exports add their own (hrv_sdnn, sleep_in_bed)
            alex_metrics = set(
                c.execute(
                    text(
                        "select distinct r.metric from wearable_rollup r join profile p on p.id = r.profile_id"
                        " where p.slug = 'alex'"
                    )
                ).scalars()
            )
            assert {"hr_resting", "hrv_rmssd", "steps", "sleep_asleep", "vo2max"} <= alex_metrics
            # every observation keeps provenance with a page and a bbox
            missing = c.execute(
                text(
                    "select count(*) from observation o join src s on s.id = o.src_id"
                    " where s.kind = 'document_region' and (s.page_no is null or s.bbox is null)"
                )
            )
            assert missing.scalar() == 0
    finally:
        eng.dispose()
