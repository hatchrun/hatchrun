-- SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
--
-- SPDX-License-Identifier: AGPL-3.0-only

-- Two passes of the alerts (the reconciler's and the dead-letter queue's)
-- could both send the email an alert is due. A pass now claims the email
-- first: sending_until is when its claim ends, NULL while nobody is
-- sending. A pass that crashes mid-send leaves the claim to expire.
ALTER TABLE alerts ADD COLUMN sending_until bigint;
