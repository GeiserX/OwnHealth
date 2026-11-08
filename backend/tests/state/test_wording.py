"""The wording lint finds 0 imperative phrases in everything lane E writes: status text in both
languages and every identity mode, the dashboard, and check_drug for every drug in the table. It is
proven able to fail on known-bad sentences and on the CPIC quotes themselves."""

import pytest

from ownhealth import state
from ownhealth.core.access import ALL_SCOPES
from ownhealth.state.pack import load_pack
from ownhealth.state.wording import imperative_phrases, lint

from .conftest import AS_OF

BAD = [
    "You should take vitamin D in winter.",
    "Take 1000 IU daily.",
    "Stop atorvastatin before the test.",
    "Avoid codeine.",
    "Discuss with your prescriber.",
    "Do not drink alcohol.",
    "Tome omeprazol antes del desayuno.",
    "Deberías repetir la TSH.",
    "No tome codeína.",
    "Consulte a su médico.",
    "Dosis: suspenda la estatina.",
]


@pytest.mark.parametrize("sentence", BAD)
def test_lint_catches_known_bad_sentences(sentence):
    assert imperative_phrases(sentence), sentence


def test_lint_passes_our_sentences():
    for ok in ("To discuss with your prescriber.", "Flags use each lab's own printed range.",
               "Para comentar con quien lo prescribe.", "Control de TSH"):  # fmt: skip
        assert imperative_phrases(ok) == [], ok


def test_every_template_is_information_not_advice():
    p = load_pack()
    hits = [(k, lang, h) for k, t in p.templates.items() for lang, s in t.items() for h in imperative_phrases(s)]
    assert hits == []


def test_status_and_dashboard_text_has_no_imperatives(sconn, seeded):
    hits = []
    for slug in ("alex", "sam", "lucia", "carmen"):
        pid = seeded.pid(slug)
        body = state.build_state(sconn, pid, as_of=AS_OF)
        for lang in ("en", "es"):
            for mode in ("full", "pseudonymous", "minimal"):
                hits += lint(state.render(body, lang=lang, identity_mode=mode))
            hits += lint(state.dashboard(sconn, pid, as_of=AS_OF, lang=lang, scopes=ALL_SCOPES))
    assert hits == []


def test_check_drug_has_no_imperatives_outside_the_cpic_quotes(sconn, seeded):
    hits, quotes = [], 0
    for slug in ("alex", "sam"):
        for drug in sorted(load_pack().drugs):
            for lang in ("en", "es"):
                out = state.check_drug(sconn, seeded.pid(slug), drug, scopes=ALL_SCOPES, lang=lang, as_of=AS_OF)
                hits += [(slug, drug, lang, *h) for h in lint(out)]
                quotes += len(out["guidance"])
    assert hits == []
    assert quotes > 20  # the loop really produced quoted guidance


def test_the_quotes_alone_would_fail_the_lint(sconn, seeded):
    """The 0 above comes from skipping verbatim CPIC text, not from a lint that cannot see."""
    out = state.check_drug(sconn, seeded.pid("sam"), "codeine", scopes=ALL_SCOPES, as_of=AS_OF)
    assert lint(out) == []
    assert lint(out, skip_keys=frozenset()) != []  # "Use codeine label recommended ..." is an imperative


def test_the_genome_notice_is_information_not_advice():
    from ownhealth.genome.model import NOTICE

    assert imperative_phrases(NOTICE) == [], NOTICE
