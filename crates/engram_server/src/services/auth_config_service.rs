// Ticket 8: Auth/Config Migration Mapping Service
//
// Parses web.config for authentication/authorization/membership configuration
// and scans code-behind for auth-related API calls. Maps all findings to
// ASP.NET Core Identity / policy-based authorization equivalents.

use engram_graph::GraphStore;
use regex::Regex;
use serde::Serialize;
use std::sync::{Arc, LazyLock};

// ── Result structs ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct AuthConfigMap {
    pub project_id: String,
    pub file_scope: Option<String>,
    pub auth_mode: String,
    pub forms_auth: Option<FormsAuthConfig>,
    pub windows_auth: Option<WindowsAuthConfig>,
    pub location_rules: Vec<LocationAuthRule>,
    pub membership_config: Option<MembershipConfig>,
    pub role_provider: Option<RoleProviderConfig>,
    pub code_auth_checks: Vec<CodeAuthCheck>,
    pub session_auth_patterns: Vec<SessionAuthPattern>,
    pub recommendations: Vec<AuthRecommendation>,
    pub migration_complexity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FormsAuthConfig {
    pub login_url: String,
    pub default_url: String,
    pub timeout_minutes: u32,
    pub cookie_name: String,
    pub cookieless: String,
    pub require_ssl: bool,
    pub sliding_expiration: bool,
    pub protection: String,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WindowsAuthConfig {
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocationAuthRule {
    pub path: String,
    pub allow_roles: Vec<String>,
    pub allow_users: Vec<String>,
    pub deny_roles: Vec<String>,
    pub deny_users: Vec<String>,
    pub modern_attribute: String,
    pub modern_policy: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MembershipConfig {
    pub default_provider: String,
    pub provider_type: String,
    pub password_format: String,
    pub min_password_length: u32,
    pub require_email: bool,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoleProviderConfig {
    pub default_provider: String,
    pub provider_type: String,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodeAuthCheck {
    pub file_path: String,
    pub line_number: usize,
    pub check_type: String,
    pub expression: String,
    pub roles_checked: Vec<String>,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionAuthPattern {
    pub file_path: String,
    pub pattern_type: String,
    pub session_key: String,
    pub description: String,
    pub modern_equivalent: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthRecommendation {
    pub category: String,
    pub severity: String,
    pub recommendation: String,
    pub modern_pattern: String,
}

// ── Regex patterns ────────────────────────────────────────────────────────

static RE_AUTH_MODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<authentication\s+mode\s*=\s*"(Forms|Windows|None|Passport)""#).unwrap()
});

static RE_FORMS_AUTH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<forms\b([^>]*?)/>|<forms\b([^>]*?)>").unwrap());

static RE_LOCATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<location\s+path\s*=\s*"([^"]*)"[^>]*>(.*?)</location>"#).unwrap()
});

static RE_ALLOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<allow\b([^>]*?)/>"#).unwrap());

static RE_DENY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<deny\b([^>]*?)/>"#).unwrap());

static RE_MEMBERSHIP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<membership\b([^>]*?)>(.*?)</membership>").unwrap());

static RE_ROLE_MANAGER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<roleManager\b([^>]*?)>(.*?)</roleManager>").unwrap());

// Code-level auth patterns
static RE_IS_IN_ROLE_CS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:User|HttpContext\.Current\.User|Context\.User|Thread\.CurrentPrincipal)\.IsInRole\s*\(\s*"([^"]*)"\s*\)"#).unwrap()
});

static RE_IS_IN_ROLE_VB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:User|HttpContext\.Current\.User|My\.User|Context\.User)\.IsInRole\s*\(\s*"([^"]*)"\s*\)"#).unwrap()
});

static RE_IS_AUTHENTICATED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:User|HttpContext\.Current\.User|Context\.User)\.Identity\.IsAuthenticated")
        .unwrap()
});

static RE_FORMS_AUTH_API: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)FormsAuthentication\.(SetAuthCookie|RedirectFromLoginPage|SignOut|GetRedirectUrl|Authenticate|RenewTicketIfOld|Decrypt|Encrypt)",
    )
    .unwrap()
});

static RE_MEMBERSHIP_API: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Membership\.(CreateUser|ValidateUser|GetUser|DeleteUser|FindUsersByName|FindUsersByEmail|UpdateUser|GetAllUsers|GetNumberOfUsersOnline)")
        .unwrap()
});

static RE_ROLES_API: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)Roles\.(AddUserToRole|RemoveUserFromRole|GetRolesForUser|IsUserInRole|CreateRole|DeleteRole|GetAllRoles|RoleExists|GetUsersInRole)")
        .unwrap()
});

static RE_SESSION_AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)Session\s*[\[(]\s*"(User(?:Id|Name|Role|Level|Type|Info|Data|Token|Permissions?)?|IsAdmin|IsLoggedIn|LoginTime|AuthToken|CurrentUser|LoggedInUser)"\s*[\])]"#)
        .unwrap()
});

static RE_PRINCIPAL_PERMISSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\[PrincipalPermission\s*\([^)]*Role\s*=\s*"([^"]*)""#).unwrap()
});

static RE_AUTHORIZE_ATTR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\[Authorize\s*(?:\([^)]*\))?\s*\]"#).unwrap());

fn extract_xml_attr(tag: &str, attr: &str) -> String {
    let pattern = format!(r#"(?i){}\s*=\s*"([^"]*)""#, regex::escape(attr));
    Regex::new(&pattern)
        .ok()
        .and_then(|re| re.captures(tag))
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

fn extract_xml_attr_bool(tag: &str, attr: &str, default: bool) -> bool {
    let val = extract_xml_attr(tag, attr);
    if val.is_empty() {
        return default;
    }
    val.eq_ignore_ascii_case("true")
}

fn parse_csv(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ── Main analysis function ────────────────────────────────────────────────

pub fn analyze_auth_config(
    graph: &Arc<GraphStore>,
    project_id: &str,
    web_config_content: Option<&str>,
    code_files: &[(&str, &str)], // (file_path, content) pairs
) -> anyhow::Result<AuthConfigMap> {
    let mut auth_mode = "None".to_string();
    let mut forms_auth = None;
    let mut windows_auth = None;
    let mut location_rules = Vec::new();
    let mut membership_config = None;
    let mut role_provider = None;
    let mut code_auth_checks = Vec::new();
    let mut session_auth_patterns = Vec::new();
    let mut recommendations = Vec::new();

    // ── Parse web.config ──

    if let Some(config) = web_config_content {
        // Authentication mode
        if let Some(cap) = RE_AUTH_MODE.captures(config) {
            auth_mode = cap[1].to_string();
        }

        // Forms authentication details
        if let Some(cap) = RE_FORMS_AUTH.captures(config) {
            let tag = cap
                .get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str())
                .unwrap_or("");
            forms_auth = Some(FormsAuthConfig {
                login_url: extract_xml_attr(tag, "loginUrl"),
                default_url: extract_xml_attr(tag, "defaultUrl"),
                timeout_minutes: extract_xml_attr(tag, "timeout")
                    .parse()
                    .unwrap_or(30),
                cookie_name: {
                    let name = extract_xml_attr(tag, "name");
                    if name.is_empty() {
                        ".ASPXAUTH".to_string()
                    } else {
                        name
                    }
                },
                cookieless: {
                    let cl = extract_xml_attr(tag, "cookieless");
                    if cl.is_empty() {
                        "UseDeviceProfile".to_string()
                    } else {
                        cl
                    }
                },
                require_ssl: extract_xml_attr_bool(tag, "requireSSL", false),
                sliding_expiration: extract_xml_attr_bool(tag, "slidingExpiration", true),
                protection: {
                    let p = extract_xml_attr(tag, "protection");
                    if p.is_empty() {
                        "All".to_string()
                    } else {
                        p
                    }
                },
                modern_equivalent: "ASP.NET Core Identity with cookie authentication: builder.Services.AddAuthentication(CookieAuthenticationDefaults.AuthenticationScheme).AddCookie(options => { ... });".to_string(),
            });
        }

        if auth_mode.eq_ignore_ascii_case("Windows") {
            windows_auth = Some(WindowsAuthConfig {
                modern_equivalent: "ASP.NET Core Windows Authentication: builder.Services.AddAuthentication(NegotiateDefaults.AuthenticationScheme).AddNegotiate(); or builder.Services.AddAuthentication(IISDefaults.AuthenticationScheme);".to_string(),
            });
        }

        // Location-based authorization rules
        for loc_cap in RE_LOCATION.captures_iter(config) {
            let path = loc_cap[1].to_string();
            let body = &loc_cap[2];

            let mut allow_roles = Vec::new();
            let mut allow_users = Vec::new();
            let mut deny_roles = Vec::new();
            let mut deny_users = Vec::new();

            for allow_cap in RE_ALLOW.captures_iter(body) {
                let tag = &allow_cap[1];
                allow_roles.extend(parse_csv(&extract_xml_attr(tag, "roles")));
                allow_users.extend(parse_csv(&extract_xml_attr(tag, "users")));
            }

            for deny_cap in RE_DENY.captures_iter(body) {
                let tag = &deny_cap[1];
                deny_roles.extend(parse_csv(&extract_xml_attr(tag, "roles")));
                deny_users.extend(parse_csv(&extract_xml_attr(tag, "users")));
            }

            let modern_attr = build_modern_authorize_attr(&allow_roles, &allow_users, &deny_users);
            let modern_policy = build_modern_policy(&path, &allow_roles, &deny_users);

            location_rules.push(LocationAuthRule {
                path,
                allow_roles,
                allow_users,
                deny_roles,
                deny_users,
                modern_attribute: modern_attr,
                modern_policy,
            });
        }

        // Also check for global <authorization> outside <location>
        // Use a simple approach: find <authorization> blocks not inside <location>
        let config_no_locations = RE_LOCATION.replace_all(config, "");
        let global_auth_re = Regex::new(r"(?is)<authorization>(.*?)</authorization>").unwrap();
        for auth_cap in global_auth_re.captures_iter(&config_no_locations) {
            let body = &auth_cap[1];
            let mut allow_roles = Vec::new();
            let mut allow_users = Vec::new();
            let mut deny_roles = Vec::new();
            let mut deny_users = Vec::new();

            for allow_cap in RE_ALLOW.captures_iter(body) {
                let tag = &allow_cap[1];
                allow_roles.extend(parse_csv(&extract_xml_attr(tag, "roles")));
                allow_users.extend(parse_csv(&extract_xml_attr(tag, "users")));
            }
            for deny_cap in RE_DENY.captures_iter(body) {
                let tag = &deny_cap[1];
                deny_roles.extend(parse_csv(&extract_xml_attr(tag, "roles")));
                deny_users.extend(parse_csv(&extract_xml_attr(tag, "users")));
            }

            if !allow_roles.is_empty()
                || !allow_users.is_empty()
                || !deny_roles.is_empty()
                || !deny_users.is_empty()
            {
                let modern_attr =
                    build_modern_authorize_attr(&allow_roles, &allow_users, &deny_users);
                let modern_policy = build_modern_policy("(global)", &allow_roles, &deny_users);
                location_rules.push(LocationAuthRule {
                    path: "(global)".to_string(),
                    allow_roles,
                    allow_users,
                    deny_roles,
                    deny_users,
                    modern_attribute: modern_attr,
                    modern_policy,
                });
            }
        }

        // Membership provider
        if let Some(cap) = RE_MEMBERSHIP.captures(config) {
            let tag = &cap[1];
            let body = &cap[2];
            let default_provider = extract_xml_attr(tag, "defaultProvider");

            let add_re = Regex::new(r"(?is)<add\b([^>]*?)/>").unwrap();
            let mut provider_type = String::new();
            let mut pwd_format = "Hashed".to_string();
            let mut min_pwd = 7u32;
            let mut require_email = true;

            for add_cap in add_re.captures_iter(body) {
                let add_tag = &add_cap[1];
                let ptype = extract_xml_attr(add_tag, "type");
                if !ptype.is_empty() {
                    provider_type = ptype;
                }
                let fmt = extract_xml_attr(add_tag, "passwordFormat");
                if !fmt.is_empty() {
                    pwd_format = fmt;
                }
                let minl = extract_xml_attr(add_tag, "minRequiredPasswordLength");
                if let Ok(v) = minl.parse::<u32>() {
                    min_pwd = v;
                }
                require_email = extract_xml_attr_bool(add_tag, "requiresUniqueEmail", true);
            }

            membership_config = Some(MembershipConfig {
                default_provider,
                provider_type,
                password_format: pwd_format,
                min_password_length: min_pwd,
                require_email,
                modern_equivalent:
                    "ASP.NET Core Identity: builder.Services.AddIdentity<ApplicationUser, IdentityRole>(options => { options.Password.RequiredLength = N; }).AddEntityFrameworkStores<AppDbContext>();"
                        .to_string(),
            });
        }

        // Role manager
        if let Some(cap) = RE_ROLE_MANAGER.captures(config) {
            let tag = &cap[1];
            let body = &cap[2];
            let default_provider = extract_xml_attr(tag, "defaultProvider");

            let add_re = Regex::new(r"(?is)<add\b([^>]*?)/>").unwrap();
            let mut provider_type = String::new();
            for add_cap in add_re.captures_iter(body) {
                let ptype = extract_xml_attr(&add_cap[1], "type");
                if !ptype.is_empty() {
                    provider_type = ptype;
                }
            }

            role_provider = Some(RoleProviderConfig {
                default_provider,
                provider_type,
                modern_equivalent: "ASP.NET Core Identity Roles: builder.Services.AddIdentity<ApplicationUser, IdentityRole>().AddRoles<IdentityRole>();".to_string(),
            });
        }
    }

    // ── Scan code files for auth-related patterns ──

    for &(file_path, content) in code_files {
        scan_code_auth_checks(
            file_path,
            content,
            &mut code_auth_checks,
            &mut session_auth_patterns,
        );
    }

    // ── Also scan graph for auth-related state edges ──

    scan_graph_auth_patterns(graph, project_id, &mut session_auth_patterns);

    // ── Generate recommendations ──

    recommendations = build_recommendations(
        &auth_mode,
        &forms_auth,
        &membership_config,
        &location_rules,
        &code_auth_checks,
        &session_auth_patterns,
    );

    let complexity = assess_complexity(
        &auth_mode,
        &location_rules,
        &code_auth_checks,
        &session_auth_patterns,
        &membership_config,
    );

    Ok(AuthConfigMap {
        project_id: project_id.to_string(),
        file_scope: None,
        auth_mode,
        forms_auth,
        windows_auth,
        location_rules,
        membership_config,
        role_provider,
        code_auth_checks,
        session_auth_patterns,
        recommendations,
        migration_complexity: complexity,
    })
}

// ── Code scanning ─────────────────────────────────────────────────────────

fn scan_code_auth_checks(
    file_path: &str,
    content: &str,
    checks: &mut Vec<CodeAuthCheck>,
    session_patterns: &mut Vec<SessionAuthPattern>,
) {
    for (line_num, line) in content.lines().enumerate() {
        let ln = line_num + 1;

        // IsInRole checks (C# and VB)
        for cap in RE_IS_IN_ROLE_CS.captures_iter(line) {
            let role = cap[1].to_string();
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "IsInRole".to_string(),
                expression: cap[0].to_string(),
                roles_checked: vec![role.clone()],
                modern_equivalent: format!(
                    "[Authorize(Roles = \"{role}\")] or policy: options.AddPolicy(\"{role}Policy\", p => p.RequireRole(\"{role}\"));"
                ),
            });
        }
        for cap in RE_IS_IN_ROLE_VB.captures_iter(line) {
            let role = cap[1].to_string();
            if !checks
                .iter()
                .any(|c| c.line_number == ln && c.check_type == "IsInRole")
            {
                checks.push(CodeAuthCheck {
                    file_path: file_path.to_string(),
                    line_number: ln,
                    check_type: "IsInRole".to_string(),
                    expression: cap[0].to_string(),
                    roles_checked: vec![role.clone()],
                    modern_equivalent: format!(
                        "[Authorize(Roles = \"{role}\")] or policy-based authorization"
                    ),
                });
            }
        }

        // IsAuthenticated checks
        if RE_IS_AUTHENTICATED.is_match(line) {
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "IsAuthenticated".to_string(),
                expression: line.trim().to_string(),
                roles_checked: vec![],
                modern_equivalent:
                    "[Authorize] attribute or User.Identity?.IsAuthenticated in Razor component"
                        .to_string(),
            });
        }

        // FormsAuthentication API calls
        if let Some(cap) = RE_FORMS_AUTH_API.captures(line) {
            let method = &cap[1];
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "FormsAuthentication".to_string(),
                expression: cap[0].to_string(),
                roles_checked: vec![],
                modern_equivalent: map_forms_auth_api(method),
            });
        }

        // Membership API calls
        if let Some(cap) = RE_MEMBERSHIP_API.captures(line) {
            let method = &cap[1];
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "MembershipAPI".to_string(),
                expression: cap[0].to_string(),
                roles_checked: vec![],
                modern_equivalent: map_membership_api(method),
            });
        }

        // Roles API calls
        if let Some(cap) = RE_ROLES_API.captures(line) {
            let method = &cap[1];
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "RolesAPI".to_string(),
                expression: cap[0].to_string(),
                roles_checked: vec![],
                modern_equivalent: map_roles_api(method),
            });
        }

        // PrincipalPermission attribute
        if let Some(cap) = RE_PRINCIPAL_PERMISSION.captures(line) {
            let role = cap[1].to_string();
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "PrincipalPermission".to_string(),
                expression: cap[0].to_string(),
                roles_checked: vec![role.clone()],
                modern_equivalent: format!("[Authorize(Policy = \"{role}Policy\")] — PrincipalPermission is obsolete in .NET Core"),
            });
        }

        // [Authorize] attribute (already modern-ish, but note it)
        if RE_AUTHORIZE_ATTR.is_match(line) {
            checks.push(CodeAuthCheck {
                file_path: file_path.to_string(),
                line_number: ln,
                check_type: "AuthorizeAttribute".to_string(),
                expression: line.trim().to_string(),
                roles_checked: vec![],
                modern_equivalent:
                    "Already uses [Authorize] — verify it works with new auth middleware"
                        .to_string(),
            });
        }

        // Session-based auth patterns
        if let Some(cap) = RE_SESSION_AUTH.captures(line) {
            let key = cap[1].to_string();
            session_patterns.push(SessionAuthPattern {
                file_path: file_path.to_string(),
                pattern_type: "SessionAuth".to_string(),
                session_key: key.clone(),
                description: format!("Session[\"{key}\"] used for authentication state"),
                modern_equivalent: format!(
                    "Replace Session[\"{key}\"] with claims-based identity: User.FindFirst(ClaimTypes.NameIdentifier) or custom claim"
                ),
            });
        }
    }
}

fn scan_graph_auth_patterns(
    graph: &Arc<GraphStore>,
    project_id: &str,
    session_patterns: &mut Vec<SessionAuthPattern>,
) {
    use engram_graph::EdgeKind;

    // Look for state reads/writes with auth-related keys
    let auth_keys = [
        "UserId",
        "UserName",
        "UserRole",
        "IsAdmin",
        "IsLoggedIn",
        "AuthToken",
        "CurrentUser",
        "LoggedInUser",
        "UserLevel",
        "UserType",
        "UserInfo",
        "UserData",
        "UserPermissions",
        "LoginTime",
        "UserToken",
    ];

    if let Ok(state_reads) = graph.list_edges_by_kind(project_id, EdgeKind::ReadsState, 10_000) {
        for edge in &state_reads {
            for auth_key in &auth_keys {
                if edge.target_id.contains(auth_key) {
                    let already_exists = session_patterns.iter().any(|p| {
                        p.session_key.eq_ignore_ascii_case(auth_key)
                            && p.file_path == edge.source_id
                    });
                    if !already_exists {
                        session_patterns.push(SessionAuthPattern {
                            file_path: edge.source_id.clone(),
                            pattern_type: "GraphStateRead".to_string(),
                            session_key: auth_key.to_string(),
                            description: format!(
                                "State read of auth key '{}' in {}",
                                auth_key, edge.source_id
                            ),
                            modern_equivalent: format!(
                                "Replace with claims: User.FindFirst(\"{}\")",
                                auth_key
                            ),
                        });
                    }
                }
            }
        }
    }
}

// ── Modern mapping helpers ────────────────────────────────────────────────

fn build_modern_authorize_attr(
    allow_roles: &[String],
    allow_users: &[String],
    deny_users: &[String],
) -> String {
    if !allow_roles.is_empty() {
        format!("[Authorize(Roles = \"{}\")]", allow_roles.join(", "))
    } else if deny_users.iter().any(|u| u == "?") {
        "[Authorize]".to_string()
    } else if allow_users.iter().any(|u| u == "*") {
        "[AllowAnonymous]".to_string()
    } else if !allow_users.is_empty() {
        format!(
            "[Authorize(Policy = \"SpecificUsers\")] // users: {}",
            allow_users.join(", ")
        )
    } else {
        "[Authorize]".to_string()
    }
}

fn build_modern_policy(path: &str, allow_roles: &[String], deny_users: &[String]) -> String {
    let mut parts = Vec::new();
    if !allow_roles.is_empty() {
        parts.push(format!(
            "options.AddPolicy(\"{path}Access\", p => p.RequireRole({}));",
            allow_roles
                .iter()
                .map(|r| format!("\"{r}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if deny_users.iter().any(|u| u == "?") {
        parts.push("// Deny anonymous: apply [Authorize] or RequireAuthorization()".to_string());
    }
    if deny_users.iter().any(|u| u == "*") && !allow_roles.is_empty() {
        parts.push("// Deny all except allowed roles: strict role-based access".to_string());
    }
    if parts.is_empty() {
        "// Review path-specific authorization requirements".to_string()
    } else {
        parts.join("\n")
    }
}

fn map_forms_auth_api(method: &str) -> String {
    match method {
        "SetAuthCookie" => "await HttpContext.SignInAsync(CookieAuthenticationDefaults.AuthenticationScheme, principal);".to_string(),
        "RedirectFromLoginPage" => "Sign in + NavigationManager.NavigateTo(returnUrl) or HttpContext.SignInAsync + Redirect".to_string(),
        "SignOut" => "await HttpContext.SignOutAsync(CookieAuthenticationDefaults.AuthenticationScheme);".to_string(),
        "GetRedirectUrl" => "Read returnUrl from query string parameter".to_string(),
        "Authenticate" | "ValidateUser" => "await _signInManager.PasswordSignInAsync(user, password, isPersistent, lockoutOnFailure);".to_string(),
        "Decrypt" | "Encrypt" => "Use DataProtection API: IDataProtectionProvider.CreateProtector(\"Auth\")".to_string(),
        "RenewTicketIfOld" => "Handled by SlidingExpiration in cookie options: options.SlidingExpiration = true;".to_string(),
        _ => format!("Review FormsAuthentication.{method}() — no direct equivalent"),
    }
}

fn map_membership_api(method: &str) -> String {
    match method {
        "CreateUser" => "await _userManager.CreateAsync(user, password);".to_string(),
        "ValidateUser" => {
            "await _signInManager.PasswordSignInAsync(userName, password, false, false);"
                .to_string()
        }
        "GetUser" => {
            "await _userManager.FindByNameAsync(userName); or FindByIdAsync(userId);".to_string()
        }
        "DeleteUser" => "await _userManager.DeleteAsync(user);".to_string(),
        "FindUsersByName" => {
            "await _userManager.FindByNameAsync(name); or LINQ query on Users DbSet".to_string()
        }
        "FindUsersByEmail" => "await _userManager.FindByEmailAsync(email);".to_string(),
        "UpdateUser" => "await _userManager.UpdateAsync(user);".to_string(),
        "GetAllUsers" => "await _userManager.Users.ToListAsync();".to_string(),
        "GetNumberOfUsersOnline" => {
            "Implement with SignalR presence tracking or custom session tracking".to_string()
        }
        _ => format!("Review Membership.{method}() usage"),
    }
}

fn map_roles_api(method: &str) -> String {
    match method {
        "AddUserToRole" => "await _userManager.AddToRoleAsync(user, role);".to_string(),
        "RemoveUserFromRole" => "await _userManager.RemoveFromRoleAsync(user, role);".to_string(),
        "GetRolesForUser" => "await _userManager.GetRolesAsync(user);".to_string(),
        "IsUserInRole" => {
            "await _userManager.IsInRoleAsync(user, role); or User.IsInRole(role)".to_string()
        }
        "CreateRole" => "await _roleManager.CreateAsync(new IdentityRole(roleName));".to_string(),
        "DeleteRole" => "await _roleManager.DeleteAsync(role);".to_string(),
        "GetAllRoles" => "await _roleManager.Roles.ToListAsync();".to_string(),
        "RoleExists" => "await _roleManager.RoleExistsAsync(roleName);".to_string(),
        "GetUsersInRole" => "await _userManager.GetUsersInRoleAsync(role);".to_string(),
        _ => format!("Review Roles.{method}() usage"),
    }
}

fn build_recommendations(
    auth_mode: &str,
    forms_auth: &Option<FormsAuthConfig>,
    membership: &Option<MembershipConfig>,
    location_rules: &[LocationAuthRule],
    code_checks: &[CodeAuthCheck],
    session_patterns: &[SessionAuthPattern],
) -> Vec<AuthRecommendation> {
    let mut recs = Vec::new();

    if auth_mode.eq_ignore_ascii_case("Forms") {
        recs.push(AuthRecommendation {
            category: "Authentication".to_string(),
            severity: "High".to_string(),
            recommendation: "Replace FormsAuthentication with ASP.NET Core cookie authentication or Identity".to_string(),
            modern_pattern: "builder.Services.AddAuthentication(CookieAuthenticationDefaults.AuthenticationScheme).AddCookie();".to_string(),
        });
    }

    if auth_mode.eq_ignore_ascii_case("Windows") {
        recs.push(AuthRecommendation {
            category: "Authentication".to_string(),
            severity: "Medium".to_string(),
            recommendation: "Windows Authentication requires IIS hosting or Negotiate/Kerberos middleware".to_string(),
            modern_pattern: "builder.Services.AddAuthentication(NegotiateDefaults.AuthenticationScheme).AddNegotiate();".to_string(),
        });
    }

    if membership.is_some() {
        recs.push(AuthRecommendation {
            category: "UserManagement".to_string(),
            severity: "High".to_string(),
            recommendation: "Replace Membership/MembershipProvider with ASP.NET Core Identity".to_string(),
            modern_pattern: "builder.Services.AddIdentity<ApplicationUser, IdentityRole>().AddEntityFrameworkStores<AppDbContext>();".to_string(),
        });
    }

    if !location_rules.is_empty() {
        recs.push(AuthRecommendation {
            category: "Authorization".to_string(),
            severity: "High".to_string(),
            recommendation: format!("Convert {} web.config <location> rules to [Authorize] attributes or endpoint-level RequireAuthorization()", location_rules.len()),
            modern_pattern: "app.MapRazorPages().RequireAuthorization(); or [Authorize(Roles = \"Admin\")] per page/controller".to_string(),
        });
    }

    if !session_patterns.is_empty() {
        recs.push(AuthRecommendation {
            category: "SessionAuth".to_string(),
            severity: "Critical".to_string(),
            recommendation: format!(
                "Found {} session-based auth patterns — replace with claims-based identity. Session auth is a security anti-pattern.",
                session_patterns.len()
            ),
            modern_pattern: "Use ClaimsPrincipal with custom claims instead of Session for auth state".to_string(),
        });
    }

    let forms_auth_calls = code_checks
        .iter()
        .filter(|c| c.check_type == "FormsAuthentication")
        .count();
    if forms_auth_calls > 0 {
        recs.push(AuthRecommendation {
            category: "API Migration".to_string(),
            severity: "High".to_string(),
            recommendation: format!("{forms_auth_calls} FormsAuthentication API calls need migration to HttpContext.SignIn/SignOutAsync"),
            modern_pattern: "await HttpContext.SignInAsync(scheme, principal, properties);".to_string(),
        });
    }

    if let Some(fa) = forms_auth {
        if !fa.require_ssl {
            recs.push(AuthRecommendation {
                category: "Security".to_string(),
                severity: "Critical".to_string(),
                recommendation:
                    "Forms authentication cookie not marked requireSSL — enable HTTPS-only cookies"
                        .to_string(),
                modern_pattern: "options.Cookie.SecurePolicy = CookieSecurePolicy.Always;"
                    .to_string(),
            });
        }
    }

    recs
}

fn assess_complexity(
    auth_mode: &str,
    location_rules: &[LocationAuthRule],
    code_checks: &[CodeAuthCheck],
    session_patterns: &[SessionAuthPattern],
    membership: &Option<MembershipConfig>,
) -> String {
    let mut score = 0u32;

    if !auth_mode.eq_ignore_ascii_case("None") {
        score += 2;
    }
    score += location_rules.len() as u32;
    score += (code_checks.len() / 3) as u32;
    if !session_patterns.is_empty() {
        score += 3;
    }
    if membership.is_some() {
        score += 3;
    }

    if score == 0 {
        "None: no authentication/authorization detected".to_string()
    } else if score <= 3 {
        "Low: basic auth configuration, straightforward migration".to_string()
    } else if score <= 8 {
        format!("Medium: {score} auth touchpoints — plan Identity migration carefully")
    } else {
        format!(
            "High: {score} auth touchpoints — consider phased auth migration with dual-auth support during transition"
        )
    }
}

// ── Format ────────────────────────────────────────────────────────────────

pub fn format_auth_config_map(report: &AuthConfigMap) -> String {
    let mut out = String::with_capacity(4096);

    out.push_str(&format!(
        "## Auth Configuration Map: {}\n\n",
        report.project_id
    ));
    out.push_str(&format!(
        "**Auth Mode:** {} | **Complexity:** {}\n\n",
        report.auth_mode, report.migration_complexity
    ));

    // Forms auth details
    if let Some(ref fa) = report.forms_auth {
        out.push_str("### Forms Authentication\n\n");
        out.push_str(&format!("- Login URL: `{}`\n", fa.login_url));
        out.push_str(&format!(
            "- Cookie: `{}` (timeout: {}min, SSL: {}, sliding: {})\n",
            fa.cookie_name, fa.timeout_minutes, fa.require_ssl, fa.sliding_expiration
        ));
        out.push_str(&format!("- Modern: {}\n\n", fa.modern_equivalent));
    }

    if let Some(ref wa) = report.windows_auth {
        out.push_str("### Windows Authentication\n\n");
        out.push_str(&format!("- Modern: {}\n\n", wa.modern_equivalent));
    }

    // Location rules
    if !report.location_rules.is_empty() {
        out.push_str("### Authorization Rules\n\n");
        out.push_str("| Path | Allow Roles | Deny Users | Modern Attribute |\n");
        out.push_str("|---|---|---|---|\n");
        for rule in &report.location_rules {
            out.push_str(&format!(
                "| {} | {} | {} | `{}` |\n",
                rule.path,
                if rule.allow_roles.is_empty() {
                    "-".to_string()
                } else {
                    rule.allow_roles.join(", ")
                },
                if rule.deny_users.is_empty() {
                    "-".to_string()
                } else {
                    rule.deny_users.join(", ")
                },
                rule.modern_attribute,
            ));
        }
        out.push('\n');
    }

    // Membership
    if let Some(ref mc) = report.membership_config {
        out.push_str("### Membership Provider\n\n");
        out.push_str(&format!(
            "- Provider: {} (type: {})\n",
            mc.default_provider, mc.provider_type
        ));
        out.push_str(&format!(
            "- Password: format={}, minLength={}\n",
            mc.password_format, mc.min_password_length
        ));
        out.push_str(&format!("- Modern: {}\n\n", mc.modern_equivalent));
    }

    // Code-level checks
    if !report.code_auth_checks.is_empty() {
        out.push_str("### Code-Level Auth Checks\n\n");
        for check in &report.code_auth_checks {
            out.push_str(&format!(
                "- **{}** `{}:{}` — `{}`\n  Modern: {}\n",
                check.check_type,
                check.file_path,
                check.line_number,
                check.expression,
                check.modern_equivalent
            ));
        }
        out.push('\n');
    }

    // Session auth patterns
    if !report.session_auth_patterns.is_empty() {
        out.push_str("### Session-Based Auth (Anti-Pattern)\n\n");
        for sp in &report.session_auth_patterns {
            out.push_str(&format!(
                "- `Session[\"{}\"]` in {} — {}\n  Modern: {}\n",
                sp.session_key, sp.file_path, sp.description, sp.modern_equivalent
            ));
        }
        out.push('\n');
    }

    // Recommendations
    if !report.recommendations.is_empty() {
        out.push_str("### Recommendations\n\n");
        for rec in &report.recommendations {
            out.push_str(&format!(
                "- **[{}] {}**: {}\n  Pattern: `{}`\n",
                rec.severity, rec.category, rec.recommendation, rec.modern_pattern
            ));
        }
    }

    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_graph() -> Arc<GraphStore> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(GraphStore::open(dir.path()).unwrap())
    }

    #[test]
    fn test_forms_auth_extraction() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <system.web>
                    <authentication mode="Forms">
                        <forms loginUrl="~/Login.aspx" timeout="30" name=".MyAuth"
                               requireSSL="true" slidingExpiration="true" />
                    </authentication>
                </system.web>
            </configuration>
        "#;

        let result = analyze_auth_config(&graph, "test", Some(config), &[]).unwrap();
        assert_eq!(result.auth_mode, "Forms");
        assert!(result.forms_auth.is_some());
        let fa = result.forms_auth.unwrap();
        assert_eq!(fa.login_url, "~/Login.aspx");
        assert_eq!(fa.timeout_minutes, 30);
        assert_eq!(fa.cookie_name, ".MyAuth");
        assert!(fa.require_ssl);
    }

    #[test]
    fn test_location_rules() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <location path="Admin">
                    <system.web>
                        <authorization>
                            <allow roles="Admin, SuperAdmin" />
                            <deny users="*" />
                        </authorization>
                    </system.web>
                </location>
                <location path="Reports/Salary.aspx">
                    <system.web>
                        <authorization>
                            <allow roles="HR" />
                            <deny users="?" />
                        </authorization>
                    </system.web>
                </location>
            </configuration>
        "#;

        let result = analyze_auth_config(&graph, "test", Some(config), &[]).unwrap();
        assert_eq!(result.location_rules.len(), 2);

        let admin = result
            .location_rules
            .iter()
            .find(|r| r.path == "Admin")
            .unwrap();
        assert!(admin.allow_roles.contains(&"Admin".to_string()));
        assert!(admin.allow_roles.contains(&"SuperAdmin".to_string()));
        assert!(admin.deny_users.contains(&"*".to_string()));
        assert!(admin.modern_attribute.contains("Authorize"));
    }

    #[test]
    fn test_membership_provider() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <system.web>
                    <membership defaultProvider="SqlMember">
                        <providers>
                            <add name="SqlMember"
                                 type="System.Web.Security.SqlMembershipProvider"
                                 passwordFormat="Hashed"
                                 minRequiredPasswordLength="8"
                                 requiresUniqueEmail="true" />
                        </providers>
                    </membership>
                </system.web>
            </configuration>
        "#;

        let result = analyze_auth_config(&graph, "test", Some(config), &[]).unwrap();
        assert!(result.membership_config.is_some());
        let mc = result.membership_config.unwrap();
        assert_eq!(mc.default_provider, "SqlMember");
        assert_eq!(mc.min_password_length, 8);
    }

    #[test]
    fn test_code_auth_checks() {
        let graph = make_graph();
        let code = r#"
            If User.IsInRole("Admin") Then
                btnDelete.Visible = True
            End If
            If Not User.Identity.IsAuthenticated Then
                Response.Redirect("Login.aspx")
            End If
            FormsAuthentication.SetAuthCookie(userName, True)
            Membership.ValidateUser(txtUser.Text, txtPass.Text)
            Roles.AddUserToRole(userName, "Manager")
        "#;

        let result = analyze_auth_config(&graph, "test", None, &[("Page.aspx.vb", code)]).unwrap();
        assert!(result.code_auth_checks.len() >= 5);

        let role_check = result
            .code_auth_checks
            .iter()
            .find(|c| c.check_type == "IsInRole")
            .unwrap();
        assert!(role_check.roles_checked.contains(&"Admin".to_string()));

        let auth_check = result
            .code_auth_checks
            .iter()
            .find(|c| c.check_type == "IsAuthenticated");
        assert!(auth_check.is_some());
    }

    #[test]
    fn test_session_auth_patterns() {
        let graph = make_graph();
        let code = r#"
            Session("UserId") = currentUser.Id
            If Session("IsAdmin") = True Then
                Session("UserRole") = "Manager"
            End If
        "#;

        let result = analyze_auth_config(&graph, "test", None, &[("Page.aspx.vb", code)]).unwrap();
        assert!(!result.session_auth_patterns.is_empty());
        let keys: Vec<&str> = result
            .session_auth_patterns
            .iter()
            .map(|p| p.session_key.as_str())
            .collect();
        assert!(keys.contains(&"UserId"));
        assert!(keys.contains(&"IsAdmin"));
    }

    #[test]
    fn test_no_auth_config() {
        let graph = make_graph();
        let result = analyze_auth_config(&graph, "test", None, &[]).unwrap();
        assert_eq!(result.auth_mode, "None");
        assert!(result.forms_auth.is_none());
        assert!(result.location_rules.is_empty());
        assert!(result.migration_complexity.contains("None"));
    }

    #[test]
    fn test_windows_auth() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <system.web>
                    <authentication mode="Windows" />
                </system.web>
            </configuration>
        "#;

        let result = analyze_auth_config(&graph, "test", Some(config), &[]).unwrap();
        assert_eq!(result.auth_mode, "Windows");
        assert!(result.windows_auth.is_some());
    }

    #[test]
    fn test_recommendations_generated() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <system.web>
                    <authentication mode="Forms">
                        <forms loginUrl="Login.aspx" />
                    </authentication>
                    <membership defaultProvider="SqlMember">
                        <providers>
                            <add name="SqlMember" type="System.Web.Security.SqlMembershipProvider" />
                        </providers>
                    </membership>
                </system.web>
                <location path="Admin">
                    <system.web>
                        <authorization>
                            <deny users="?" />
                        </authorization>
                    </system.web>
                </location>
            </configuration>
        "#;
        let code = r#"Session("UserId") = user.Id"#;

        let result =
            analyze_auth_config(&graph, "test", Some(config), &[("Page.aspx.vb", code)]).unwrap();
        assert!(result.recommendations.len() >= 3);

        let categories: Vec<&str> = result
            .recommendations
            .iter()
            .map(|r| r.category.as_str())
            .collect();
        assert!(categories.contains(&"Authentication"));
        assert!(categories.contains(&"UserManagement"));
        assert!(categories.contains(&"SessionAuth"));
    }

    #[test]
    fn test_role_manager() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <system.web>
                    <roleManager enabled="true" defaultProvider="SqlRole">
                        <providers>
                            <add name="SqlRole" type="System.Web.Security.SqlRoleProvider" />
                        </providers>
                    </roleManager>
                </system.web>
            </configuration>
        "#;

        let result = analyze_auth_config(&graph, "test", Some(config), &[]).unwrap();
        assert!(result.role_provider.is_some());
        let rp = result.role_provider.unwrap();
        assert_eq!(rp.default_provider, "SqlRole");
    }

    #[test]
    fn test_format_output() {
        let graph = make_graph();
        let config = r#"
            <configuration>
                <system.web>
                    <authentication mode="Forms">
                        <forms loginUrl="Login.aspx" timeout="30" />
                    </authentication>
                </system.web>
            </configuration>
        "#;

        let result = analyze_auth_config(&graph, "test", Some(config), &[]).unwrap();
        let formatted = format_auth_config_map(&result);
        assert!(formatted.contains("Auth Configuration Map"));
        assert!(formatted.contains("Forms Authentication"));
    }
}
