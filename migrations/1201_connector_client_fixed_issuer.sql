-- A client with a secret belongs to one server. The secret goes to that server's token endpoint, and
-- never to a server a person names at launch. A public client (no secret) may still serve every server
-- of its vendor (issuer null).
alter table connector_client add constraint connector_client_secret_needs_issuer
  check (client_secret_id is null or issuer is not null);
