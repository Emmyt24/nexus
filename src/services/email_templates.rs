use chrono::{DateTime, Utc};

pub struct EmailContent {
    pub subject: String,
    pub text_body: String,
    pub html_body: String,
}

fn wrap_html(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <title>{}</title>
  </head>
    <body style="margin:0; padding:0; background:#eef2f7;">
        <div style="display:none; font-size:1px; color:#eef2f7; line-height:1px; max-height:0; max-width:0; opacity:0; overflow:hidden;">{}</div>
        <table role="presentation" width="100%" cellspacing="0" cellpadding="0" style="background:#eef2f7;">
            <tr>
                <td align="center" style="padding:28px 16px;">
                    <table role="presentation" width="640" cellspacing="0" cellpadding="0" style="width:640px; max-width:100%; background:#ffffff; border-radius:16px; box-shadow:0 10px 30px rgba(15,23,42,0.12); overflow:hidden;">
                        <tr>
                            <td style="background:linear-gradient(90deg,#1d4ed8,#38bdf8); height:6px;"></td>
                        </tr>
                        <tr>
                            <td style="padding:24px 28px 8px 28px; font-family:Arial, sans-serif;">
                                <div style="font-size:12px; letter-spacing:2px; text-transform:uppercase; color:#64748b;">NexusCare</div>
                                <h1 style="margin:8px 0 0 0; font-size:24px; color:#0f172a;">{}</h1>
                            </td>
                        </tr>
                        <tr>
                            <td style="padding:8px 28px 24px 28px; font-family:Arial, sans-serif; color:#334155; line-height:1.7; font-size:15px;">
                                {}
                            </td>
                        </tr>
                        <tr>
                            <td style="padding:0 28px 24px 28px;">
                                <div style="height:1px; background:#e2e8f0;"></div>
                            </td>
                        </tr>
                        <tr>
                            <td style="padding:0 28px 28px 28px; font-family:Arial, sans-serif; color:#94a3b8; font-size:12px;">
                                If you did not request this email, you can safely ignore it.
                            </td>
                        </tr>
                    </table>
                    <div style="margin-top:16px; font-family:Arial, sans-serif; color:#94a3b8; font-size:12px;">NexusCare Platform</div>
                </td>
            </tr>
        </table>
  </body>
</html>"#,
        title, title, title, body
    )
}

fn format_timestamp(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

pub fn hospital_registration_submitted(hospital_name: &str) -> EmailContent {
    let subject = "Hospital Registration Submitted - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nYour hospital registration has been submitted successfully. Our team will review your details and notify you of the outcome.\n\nThank you,\nNexusCare",
        hospital_name
    );
    let html_body = wrap_html(
        "Hospital Registration Submitted",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">Your hospital registration has been submitted successfully. Our team will review your details and notify you of the outcome.</p>
             <p style=\"margin:0;\">Thank you,<br/>NexusCare</p>",
            hospital_name
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn hospital_registration_approved(
    hospital_name: &str,
    approved_at: DateTime<Utc>,
) -> EmailContent {
    let subject = "Hospital Registration Approved - NexusCare".to_string();
    let text_body = format!(
        "Congratulations! Your hospital '{}' has been approved.\n\nApproval Date: {}\n\nYou can now access the platform and start creating shifts.\n\nNexusCare",
        hospital_name,
        format_timestamp(approved_at)
    );
    let html_body = wrap_html(
        "Hospital Registration Approved",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Congratulations! Your hospital '<strong>{}</strong>' has been approved.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Approval Date:</strong> {}</p>
             <p style=\"margin:0 0 12px 0;\">You can now access the platform and start creating shifts.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            hospital_name,
            format_timestamp(approved_at)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn hospital_registration_rejected(hospital_name: &str, reason: &str) -> EmailContent {
    let subject = "Hospital Registration Update - NexusCare".to_string();
    let text_body = format!(
        "Your hospital '{}' could not be approved at this time.\n\nReason: {}\n\nIf you have questions, contact support.\n\nNexusCare",
        hospital_name,
        reason
    );
    let html_body = wrap_html(
        "Hospital Registration Update",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Your hospital '<strong>{}</strong>' could not be approved at this time.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Reason:</strong> {}</p>
             <p style=\"margin:0 0 12px 0;\">If you have questions, contact support.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            hospital_name,
            reason
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn clinician_welcome(first_name: Option<&str>) -> EmailContent {
    let subject = "Welcome to NexusCare".to_string();
    let greeting = first_name.unwrap_or("there");
    let text_body = format!(
        "Hello {},\n\nYour clinician account has been created successfully. Complete your profile to start receiving shift opportunities.\n\nNexusCare",
        greeting
    );
    let html_body = wrap_html(
        "Welcome to NexusCare",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">Your clinician account has been created successfully. Complete your profile to start receiving shift opportunities.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            greeting
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn shift_created(
    hospital_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
) -> EmailContent {
    let subject = "Shift Created - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nYour shift '{}' has been created and broadcast.\nStart: {}\n\nNexusCare",
        hospital_name,
        role_title,
        format_timestamp(scheduled_start)
    );
    let html_body = wrap_html(
        "Shift Created",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">Your shift '<strong>{}</strong>' has been created and broadcast.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Start:</strong> {}</p>
             <p style=\"margin:0;\">NexusCare</p>",
            hospital_name,
            role_title,
            format_timestamp(scheduled_start)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn shift_assigned_clinician(
    clinician_name: &str,
    hospital_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
) -> EmailContent {
    let subject = "Shift Assignment - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nYou have been assigned to a shift at {}.\nRole: {}\nStart: {}\n\nNexusCare",
        clinician_name,
        hospital_name,
        role_title,
        format_timestamp(scheduled_start)
    );
    let html_body = wrap_html(
        "Shift Assignment",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">You have been assigned to a shift at <strong>{}</strong>.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Role:</strong> {}<br/><strong>Start:</strong> {}</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_name,
            hospital_name,
            role_title,
            format_timestamp(scheduled_start)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to each eligible clinician when a shift is broadcast or re-broadcast

pub fn shift_broadcast(
    clinician_first_name: &str,
    hospital_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
    priority: crate::models::shift::ShiftPriority,
) -> EmailContent {
    use crate::models::shift::ShiftPriority;
    let (subject, label) = match priority {
        ShiftPriority::Stat => (
            "STAT shift available - NexusCare".to_string(),
            "🚨 STAT shift",
        ),
        ShiftPriority::Urgent => (
            "Urgent shift available - NexusCare".to_string(),
            "⚠️ Urgent shift",
        ),
        ShiftPriority::Normal => (
            "New shift available - NexusCare".to_string(),
            "📍 New shift",
        ),
        ShiftPriority::Scheduled => (
            "Scheduled shift available - NexusCare".to_string(),
            "📅 Scheduled shift",
        ),
    };

    let text_body = format!(
        "Hello {},\n\n{}: {} at {}.\nStarts: {}\n\nOpen the NexusCare app to view details and express interest.\n\nNexusCare",
        clinician_first_name,
        label,
        role_title,
        hospital_name,
        format_timestamp(scheduled_start)
    );
    let html_body = wrap_html(
        label,
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\"><strong>{}:</strong> {} at <strong>{}</strong>.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Starts:</strong> {}</p>
             <p style=\"margin:0 0 12px 0;\">Open the NexusCare app to view details and express interest.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_first_name,
            label,
            role_title,
            hospital_name,
            format_timestamp(scheduled_start)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to the hospital when a worker requests a GPS-fallback

pub fn clockin_approval_requested(clinician_name: &str, role_title: &str) -> EmailContent {
    let subject = "Manual Clock-In Requested - NexusCare".to_string();
    let text_body = format!(
        "{} has requested a manual clock-in for the {} shift because their GPS fix is inaccurate.\n\nReview the photo in the NexusCare app and approve or deny.\n\nNexusCare",
        clinician_name, role_title
    );
    let html_body = wrap_html(
        "Manual Clock-In Requested",
        &format!(
            "<p style=\"margin:0 0 12px 0;\"><strong>{}</strong> has requested a manual clock-in for the <strong>{}</strong> shift because their GPS fix is inaccurate.</p>
             <p style=\"margin:0 0 12px 0;\">Review the photo in the NexusCare app and approve or deny.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_name, role_title
        ),
    );
    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to the worker when their GPS-fallback request is approved

pub fn clockin_approval_approved(clinician_first_name: &str, role_title: &str) -> EmailContent {
    let subject = "Manual Clock-In Approved - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nYour manual clock-in request for the {} shift was approved.\nYou can now clock in via the NexusCare app.\n\nNexusCare",
        clinician_first_name, role_title
    );
    let html_body = wrap_html(
        "Manual Clock-In Approved",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">Your manual clock-in request for the <strong>{}</strong> shift was approved.</p>
             <p style=\"margin:0 0 12px 0;\">You can now clock in via the NexusCare app.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_first_name, role_title
        ),
    );
    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to the worker when their GPS-fallback request is denied

pub fn clockin_approval_denied(
    clinician_first_name: &str,
    role_title: &str,
    notes: Option<&str>,
) -> EmailContent {
    let subject = "Manual Clock-In Denied - NexusCare".to_string();
    let notes_line = notes
        .filter(|s| !s.trim().is_empty())
        .map(|n| format!("\nNotes: {n}"))
        .unwrap_or_default();
    let text_body = format!(
        "Hello {},\n\nYour manual clock-in request for the {} shift was denied.{}\n\nContact the hospital admin if you believe this is in error.\n\nNexusCare",
        clinician_first_name, role_title, notes_line
    );
    let html_body = wrap_html(
        "Manual Clock-In Denied",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">Your manual clock-in request for the <strong>{}</strong> shift was denied.{}</p>
             <p style=\"margin:0 0 12px 0;\">Contact the hospital admin if you believe this is in error.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_first_name, role_title,
            notes.filter(|s| !s.trim(). is_empty())
                .map(|n| format!("<br/><strong>Notes:</strong> {n}"))
                .unwrap_or_default()
        ),
    );
    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to the worker when their handover is auto-approved after

pub fn handover_auto_approved(clinician_first_name: &str, role_title: &str) -> EmailContent {
    let subject = "Handover Auto-Approved - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nYour handover for the {} shift was auto-approved after 48 hours without hospital action.\nPayment processing can proceed.\n\nNexusCare",
        clinician_first_name, role_title
    );
    let html_body = wrap_html(
        "Handover Auto-Approved",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">Your handover for the <strong>{}</strong> shift was auto-approved after 48 hours without hospital action.</p>
             <p style=\"margin:0 0 12px 0;\">Payment processing can proceed.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_first_name, role_title
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to the hospital when a shift offer to a clinician expires

pub fn shift_offer_expired(role_title: &str) -> EmailContent {
    let subject = "Shift Offer Expired - NexusCare".to_string();
    let text_body = format!(
        "The offer for {} expired because the worker did not respond within 30 minutes.\nYou can select the next ranked candidate.\n\nNexusCare",
        role_title
    );
    let html_body = wrap_html(
        "Shift Offer Expired",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">The offer for <strong>{}</strong> expired because the worker did not respond within 30 minutes.</p>
             <p style=\"margin:0 0 12px 0;\">You can select the next ranked candidate.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            role_title
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to the hospital when a worker declines a shift offer

pub fn shift_offer_declined(
    role_title: &str,
    scheduled_start: DateTime<Utc>,
    reason: Option<&str>,
) -> EmailContent {
    let subject = "Shift Offer Declined - NexusCare".to_string();
    let reason_line = reason
        .filter(|s| !s.trim().is_empty())
        .map(|r| format!("\nReason: {r}"))
        .unwrap_or_default();
    let text_body = format!(
        "The shift offer was declined.\nRole: {}\nStart: {}{}\n\nYou can select the next ranked candidate.\n\nNexusCare",
        role_title,
        format_timestamp(scheduled_start),
        reason_line
    );
    let html_body = wrap_html(
        "Shift Offer Declined",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">The shift offer was declined.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Role:</strong> {}<br/>\
                                              <strong>Start:</strong> {}{}</p>
             <p style=\"margin:0 0 12px 0;\">You can select the next ranked candidate.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            role_title,
            format_timestamp(scheduled_start),
            reason
                .filter(|s| !s.trim().is_empty())
                .map(|r| format!("<br/><strong>Reason:</strong> {r}"))
                .unwrap_or_default()
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Sent to a clinician when a hospital sends them a shift offer

pub fn shift_offered(
    clinician_first_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> EmailContent {
    let subject = "Shift Offer - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nYou have a new shift offer.\nRole: {}\nStart: {}\nOffer expires: {}\n\nOpen the NexusCare app to accept or decline.\n\nNexusCare",
        clinician_first_name,
        role_title,
        format_timestamp(scheduled_start),
        format_timestamp(expires_at)
    );
    let html_body = wrap_html(
        "Shift Offer",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">You have a new shift offer.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Role:</strong> {}<br/>\
                                              <strong>Start:</strong> {}<br/>\
                                              <strong>Offer expires:</strong> {}</p>
             <p style=\"margin:0 0 12px 0;\">Open the NexusCare app to accept or decline.</p>
             <p style=\"margin:0;\">NexusCare</p>",
            clinician_first_name,
            role_title,
            format_timestamp(scheduled_start),
            format_timestamp(expires_at)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn shift_assigned_hospital(
    hospital_name: &str,
    clinician_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
) -> EmailContent {
    let subject = "Clinician Assigned - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\n{} has been assigned to your shift.\nRole: {}\nStart: {}\n\nNexusCare",
        hospital_name,
        clinician_name,
        role_title,
        format_timestamp(scheduled_start)
    );
    let html_body = wrap_html(
        "Clinician Assigned",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\"><strong>{}</strong> has been assigned to your shift.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Role:</strong> {}<br/><strong>Start:</strong> {}</p>
             <p style=\"margin:0;\">NexusCare</p>",
            hospital_name,
            clinician_name,
            role_title,
            format_timestamp(scheduled_start)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn shift_cancelled(
    recipient_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
    reason: &str,
) -> EmailContent {
    let subject = "Shift Cancelled - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nThe shift '{}' scheduled for {} has been cancelled.\nReason: {}\n\nNexusCare",
        recipient_name,
        role_title,
        format_timestamp(scheduled_start),
        reason
    );
    let html_body = wrap_html(
        "Shift Cancelled",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">The shift '<strong>{}</strong>' scheduled for {} has been cancelled.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>Reason:</strong> {}</p>
             <p style=\"margin:0;\">NexusCare</p>",
            recipient_name,
            role_title,
            format_timestamp(scheduled_start),
            reason
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn shift_rescheduled(
    recipient_name: &str,
    role_title: &str,
    scheduled_start: DateTime<Utc>,
) -> EmailContent {
    let subject = "Shift Rescheduled - NexusCare".to_string();
    let text_body = format!(
        "Hello {},\n\nThe shift '{}' has been rescheduled.\nNew Start: {}\n\nNexusCare",
        recipient_name,
        role_title,
        format_timestamp(scheduled_start)
    );
    let html_body = wrap_html(
        "Shift Rescheduled",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hello {},</p>
             <p style=\"margin:0 0 12px 0;\">The shift '<strong>{}</strong>' has been rescheduled.</p>
             <p style=\"margin:0 0 12px 0;\"><strong>New Start:</strong> {}</p>
             <p style=\"margin:0;\">NexusCare</p>",
            recipient_name,
            role_title,
            format_timestamp(scheduled_start)
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn email_otp(code: &str, expires_in_minutes: i64) -> EmailContent {
    let subject = "Your NexusCare verification code".to_string();
    let text_body = format!(
        "Your verification code is {}. It expires in {} minutes.",
        code, expires_in_minutes
    );
    let html_body = wrap_html(
        "Verification Code",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Your verification code is:</p>
             <div style=\"display:inline-block; padding:12px 16px; border-radius:10px; background:#eff6ff; color:#1d4ed8; font-size:22px; font-weight:bold; letter-spacing:2px;\">{}</div>
             <p style=\"margin:12px 0 0 0;\">It expires in {} minutes.</p>",
            code,
            expires_in_minutes
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

pub fn password_reset(reset_link: &str) -> EmailContent {
    let subject = "Reset your NexusCare password".to_string();
    let text_body = format!(
        "Click the link below to reset your password. It expires in 1 hour.\n\n{}\n\nIf you did not request this, ignore this email.",
        reset_link
    );
    let html_body = wrap_html(
        "Reset your password",
        &format!(
            "<p style=\"margin:0 0 16px 0;\">Click the button below to reset your password. It expires in 1 hour.</p>
             <p style=\"margin:0 0 16px 0;\"><a href=\"{}\" style=\"display:inline-block; background:#2563eb; color:#ffffff; text-decoration:none; padding:10px 16px; border-radius:8px; font-weight:bold;\">Reset Password</a></p>
             <p style=\"margin:0; color:#64748b; font-size:13px;\">If the button doesn't work, copy and paste this link into your browser:</p>
             <p style=\"margin:6px 0 0 0; word-break:break-all;\"><a href=\"{}\" style=\"color:#2563eb;\">{}</a></p>",
            reset_link,
            reset_link,
            reset_link
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}

/// Invitation sent to a newly-created sub-admin with their login credentials.
pub fn admin_invite(
    first_name: &str,
    role: &str,
    email: &str,
    temp_password: &str,
    login_url: &str,
) -> EmailContent {
    let role_label = role.replace('_', " ");
    let subject = "You've been invited to the NexusCare admin console".to_string();
    let text_body = format!(
        "Hi {first_name},\n\nYou have been invited to the NexusCare admin console as {role_label}.\n\nSign in at: {login_url}\nEmail: {email}\nTemporary password: {temp_password}\n\nPlease sign in and change your password.\n"
    );
    let html_body = wrap_html(
        "Admin invitation",
        &format!(
            "<p style=\"margin:0 0 12px 0;\">Hi {first_name},</p>
             <p style=\"margin:0 0 16px 0;\">You have been invited to the NexusCare admin console as <strong>{role_label}</strong>.</p>
             <table style=\"margin:0 0 16px 0; font-size:14px;\">
               <tr><td style=\"padding:2px 8px 2px 0; color:#64748b;\">Email</td><td><strong>{email}</strong></td></tr>
               <tr><td style=\"padding:2px 8px 2px 0; color:#64748b;\">Temporary password</td><td><strong>{temp_password}</strong></td></tr>
             </table>
             <p style=\"margin:0 0 16px 0;\"><a href=\"{login_url}\" style=\"display:inline-block; background:#2563eb; color:#ffffff; text-decoration:none; padding:10px 16px; border-radius:8px; font-weight:bold;\">Sign in</a></p>
             <p style=\"margin:0; color:#64748b; font-size:13px;\">Please sign in and change your password as soon as possible.</p>"
        ),
    );

    EmailContent {
        subject,
        text_body,
        html_body,
    }
}
