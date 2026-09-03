#!/usr/bin/env bash
# Seed a virtual shift that is ready to join right now, and print the two JWTs
# and the shift ID you need to test it. Development only.
#
#   JWT_SECRET=... DATABASE_URL=postgres://... ./dev/seed-consult-fixture.sh
#
# Creates: an approved, funded hospital · a hospital admin · a health worker
# with a clinician profile · a virtual shift starting now, assigned to that
# worker with an accepted assignment.
set -euo pipefail

: "${JWT_SECRET:?set JWT_SECRET (same value the server runs with)}"
: "${DATABASE_URL:?set DATABASE_URL}"

HERE=$(cd "$(dirname "$0")" && pwd)
psql_() { psql -qtAX "$DATABASE_URL" -c "$1"; }

SUF=$(date +%s)

HOSPITAL=$(psql_ "INSERT INTO hospitals
    (name, registration_number, email, address, phone_number, admin_registration_status)
  VALUES ('Consult Fixture $SUF', 'RC-$SUF', 'hospital-$SUF@example.test',
          '1 Test Road', '08000000000', 'approved')
  RETURNING id;")

# A location and a funded wallet, so shift creation through the API also works.
psql_ "INSERT INTO hospital_locations (hospital_id, latitude, longitude, clock_in_radius_meters)
       VALUES ('$HOSPITAL', 6.5244, 3.3792, 100);" > /dev/null
psql_ "INSERT INTO hospital_wallets (hospital_id, balance_kobo)
       VALUES ('$HOSPITAL', 5000000000);" > /dev/null

ADMIN_EMAIL="admin-$SUF@example.test"
ADMIN=$(psql_ "INSERT INTO users (email, first_name, last_name, password_hash, role, hospital_id)
  VALUES ('$ADMIN_EMAIL', 'Ada', 'Okoro', 'not-a-real-hash', 'hospital_admin', '$HOSPITAL')
  RETURNING id;")

WORKER_EMAIL="worker-$SUF@example.test"
WORKER=$(psql_ "INSERT INTO users (email, first_name, last_name, password_hash, role)
  VALUES ('$WORKER_EMAIL', 'Amina', 'Bello', 'not-a-real-hash', 'health_worker')
  RETURNING id;")

CLINICIAN=$(psql_ "INSERT INTO clinicians (user_id, first_name, last_name, specialty, role_title)
  VALUES ('$WORKER', 'Amina', 'Bello', 'emergency_medicine', 'Emergency Doctor')
  RETURNING id;")

# Starting now, so it sits inside the +/- 60 minute consultation window.
SHIFT=$(psql_ "INSERT INTO shifts
    (hospital_id, role_category, role_title, shift_type, status,
     scheduled_start, duration_hours, scheduled_end, assigned_clinician_id,
     pay_type, rate_kobo_per_hour, grand_total_kobo, created_by, virtual_link)
  VALUES ('$HOSPITAL', 'doctor', 'Emergency Doctor', 'virtual', 'assigned',
          NOW(), 4, NOW() + INTERVAL '4 hours', '$CLINICIAN',
          'hourly_rate', 800000, 3200000, '$ADMIN',
          '${APP_PUBLIC_BASE_URL:-https://app.nexuscare.com}/consults/' ||
          gen_random_uuid()::text)
  RETURNING id;")

# The link is written at creation time from the shift's own id, so fix it up.
psql_ "UPDATE shifts
          SET virtual_link = '${APP_PUBLIC_BASE_URL:-https://app.nexuscare.com}/consults/$SHIFT'
        WHERE id = '$SHIFT';" > /dev/null

psql_ "INSERT INTO shift_assignments (shift_id, clinician_id, status, expires_at, responded_at)
       VALUES ('$SHIFT', '$CLINICIAN', 'accepted', NOW() + INTERVAL '1 day', NOW());" > /dev/null

WORKER_JWT=$("$HERE/mint-jwt.sh" "$WORKER" "$WORKER_EMAIL" health_worker)
ADMIN_JWT=$("$HERE/mint-jwt.sh"  "$ADMIN"  "$ADMIN_EMAIL"  hospital_admin "$HOSPITAL")

cat <<EOF

  Shift (virtual, assigned, starting now)
    SHIFT_ID=$SHIFT
    room_name=shift-$SHIFT

  Health worker — the assigned clinician
    user_id=$WORKER
    identity=u:$WORKER
    WORKER_JWT=$WORKER_JWT

  Hospital admin — same hospital
    user_id=$ADMIN
    ADMIN_JWT=$ADMIN_JWT

  Try it:
    curl -s -X POST http://localhost:8080/api/v1/shifts/$SHIFT/consult/token \\
      -H "Authorization: Bearer \$WORKER_JWT" \\
      -H 'Content-Type: application/json' -d '{}' | jq

  Simulate the join webhook (mock mode accepts unsigned bodies):
    curl -s -X POST http://localhost:8080/api/v1/webhooks/livekit \\
      -H 'Content-Type: application/webhook+json' \\
      -d '{"event":"participant_joined","id":"evt-$SUF","createdAt":'\$(date +%s)',
           "room":{"name":"shift-$SHIFT","sid":"RM_test"},
           "participant":{"identity":"u:$WORKER","sid":"PA_test","name":"Dr Test"}}'

EOF
