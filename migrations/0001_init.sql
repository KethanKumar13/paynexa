-- Paynexa initial schema. Money is BIGINT cents; IDs are UUID v4.

CREATE TABLE businesses (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name       text NOT NULL CHECK (length(name) > 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE api_keys (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id uuid NOT NULL REFERENCES businesses (id),
    key_prefix  text NOT NULL UNIQUE,
    key_hash    bytea NOT NULL,
    revoked_at  timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE customers (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id uuid NOT NULL REFERENCES businesses (id),
    name        text NOT NULL CHECK (length(name) > 0),
    email       text NOT NULL CHECK (length(email) > 0),
    created_at  timestamptz NOT NULL DEFAULT now(),
    -- target for composite FKs so an invoice can never reference another tenant's customer
    UNIQUE (business_id, id)
);
CREATE INDEX customers_business_created_idx ON customers (business_id, created_at DESC, id DESC);

CREATE TABLE invoices (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id uuid NOT NULL REFERENCES businesses (id),
    customer_id uuid NOT NULL,
    status      text NOT NULL CHECK (status IN ('open', 'processing', 'paid', 'void', 'uncollectible')),
    total_cents bigint NOT NULL CHECK (total_cents >= 0),
    currency    text NOT NULL DEFAULT 'USD' CHECK (currency = 'USD'),
    due_date    date NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (business_id, customer_id) REFERENCES customers (business_id, id),
    UNIQUE (business_id, id)
);
CREATE INDEX invoices_business_status_created_idx ON invoices (business_id, status, created_at DESC, id DESC);
CREATE INDEX invoices_business_created_idx ON invoices (business_id, created_at DESC, id DESC);

CREATE TABLE invoice_line_items (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id        uuid NOT NULL REFERENCES invoices (id),
    position          integer NOT NULL CHECK (position >= 0),
    description       text NOT NULL CHECK (length(description) > 0),
    quantity          bigint NOT NULL CHECK (quantity > 0),
    unit_amount_cents bigint NOT NULL CHECK (unit_amount_cents >= 0),
    line_total_cents  bigint NOT NULL CHECK (line_total_cents = quantity * unit_amount_cents),
    UNIQUE (invoice_id, position)
);

CREATE TABLE payment_attempts (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    invoice_id    uuid NOT NULL,
    business_id   uuid NOT NULL,
    card_token    text NOT NULL,
    amount_cents  bigint NOT NULL CHECK (amount_cents >= 0),
    status        text NOT NULL CHECK (status IN ('pending', 'succeeded', 'failed', 'unknown')),
    psp_reference text,
    failure_code  text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (business_id, invoice_id) REFERENCES invoices (business_id, id)
);
CREATE INDEX payment_attempts_invoice_idx ON payment_attempts (invoice_id, created_at);
-- at most one in-flight attempt per invoice (backstop for the invoice row lock)
CREATE UNIQUE INDEX payment_attempts_one_in_flight_idx ON payment_attempts (invoice_id)
    WHERE status IN ('pending', 'unknown');
CREATE INDEX payment_attempts_sweeper_idx ON payment_attempts (created_at)
    WHERE status IN ('pending', 'unknown');

CREATE TABLE idempotency_records (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id        uuid NOT NULL REFERENCES businesses (id),
    operation          text NOT NULL,
    key                text NOT NULL CHECK (length(key) BETWEEN 1 AND 255),
    request_hash       bytea NOT NULL,
    payment_attempt_id uuid REFERENCES payment_attempts (id),
    response_status    integer,
    response_body      jsonb,
    created_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (business_id, operation, key)
);

CREATE TABLE webhook_endpoints (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id    uuid NOT NULL REFERENCES businesses (id),
    url            text NOT NULL,
    signing_secret text NOT NULL,
    active         boolean NOT NULL DEFAULT true,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_endpoints_business_idx ON webhook_endpoints (business_id);

CREATE TABLE webhook_events (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    business_id     uuid NOT NULL REFERENCES businesses (id),
    endpoint_id     uuid NOT NULL REFERENCES webhook_endpoints (id),
    event_type      text NOT NULL CHECK (event_type IN ('invoice.created', 'invoice.paid', 'invoice.payment_failed')),
    payload         jsonb NOT NULL,
    status          text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'delivered', 'failed')),
    attempt_count   integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    last_error      text,
    delivered_at    timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX webhook_events_due_idx ON webhook_events (next_attempt_at) WHERE status = 'pending';
CREATE INDEX webhook_events_business_status_idx ON webhook_events (business_id, status, created_at DESC);
