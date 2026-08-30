use serde::{Deserialize, Serialize};

/// Categories are stable protocol values shared with the frontend smart folders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SmartCategory {
    AppleConnect,
    AppleAds,
    Social,
    Ads,
    Inbox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Classification {
    pub category: SmartCategory,
    pub is_ad: bool,
    pub confidence: u8,
}

/// Deterministic, local-only classification. It intentionally never uploads message content or
/// uses a remote model. The rules are conservative: advertising is tagged and can be hidden by
/// the UI, while security notices remain visible in Apple Connect.
pub fn classify(sender: &str, subject: &str, has_list_unsubscribe: bool) -> Classification {
    let sender = sender.trim().to_ascii_lowercase();
    let subject = subject.trim().to_ascii_lowercase();
    let sender_domain = sender
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim_end_matches('.'))
        .unwrap_or_default();
    let apple_sender = sender_domain == "apple.com" || sender_domain.ends_with(".apple.com");
    let security_subject = [
        "security",
        "sign in",
        "signin",
        "登录",
        "安全",
        "account alert",
        "账户提醒",
    ]
    .iter()
    .any(|term| subject.contains(term));
    let marketing_subject = [
        "offer",
        "sale",
        "new this month",
        "promotion",
        "广告",
        "优惠",
        "活动",
    ]
    .iter()
    .any(|term| subject.contains(term));

    if apple_sender && security_subject {
        return Classification {
            category: SmartCategory::AppleConnect,
            is_ad: false,
            confidence: 98,
        };
    }
    if apple_sender && marketing_subject {
        return Classification {
            category: SmartCategory::AppleAds,
            is_ad: true,
            confidence: 96,
        };
    }

    let social_sender = [
        "linkedin.com",
        "facebook.com",
        "instagram.com",
        "twitter.com",
        "x.com",
    ]
    .iter()
    .any(|domain| sender_domain == *domain || sender_domain.ends_with(&format!(".{domain}")));
    if social_sender {
        return Classification {
            category: SmartCategory::Social,
            is_ad: false,
            confidence: 90,
        };
    }

    if has_list_unsubscribe || marketing_subject {
        return Classification {
            category: SmartCategory::Ads,
            is_ad: true,
            confidence: if has_list_unsubscribe { 92 } else { 70 },
        };
    }

    Classification {
        category: SmartCategory::Inbox,
        is_ad: false,
        confidence: 55,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_apple_security_mail_visible() {
        let result = classify(
            "no-reply@apple.com",
            "Your Apple Account was used to sign in",
            false,
        );
        assert_eq!(result.category, SmartCategory::AppleConnect);
        assert!(!result.is_ad);
    }

    #[test]
    fn marks_marketing_mail_as_advertising() {
        let result = classify("news@apple.com", "Discover what is new this month", true);
        assert_eq!(result.category, SmartCategory::AppleAds);
        assert!(result.is_ad);
    }

    #[test]
    fn does_not_trust_brand_names_inside_unrelated_domains() {
        let apple = classify("security@apple.example.com", "New offer", true);
        assert_eq!(apple.category, SmartCategory::Ads);

        let social = classify("alerts@notlinkedin.com", "New message", false);
        assert_eq!(social.category, SmartCategory::Inbox);
    }
}
