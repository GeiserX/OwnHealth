-- More UI languages. app_user.locale takes every code planned for 1.0 (de, fr, ca, it, pt besides es
-- and en), so a language joins the registry (ownhealth_core::langs::LANGS) with no migration; the
-- server still stores only a registry language. api_token.lang stays es/en: MCP answers in es or en.
-- check_mirror.rs keeps this list equal to ownhealth_core::langs::LOCALES.
alter table app_user drop constraint app_user_locale_check;
alter table app_user add constraint app_user_locale_check
  check (locale in ('es', 'en', 'de', 'fr', 'ca', 'it', 'pt'));
