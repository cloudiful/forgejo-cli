use forgejo_api::Forgejo;

pub mod yaml;

use crate::repo::RepoName;

#[derive(serde::Deserialize, Default, Debug)]
pub struct TemplateMetadata {
    pub labels: Option<Vec<String>>,
    pub r#ref: Option<String>,
}

pub struct MarkdownTemplate {
    pub metadata: TemplateMetadata,
    pub body: String,
}

impl MarkdownTemplate {
    pub fn new(md: &str) -> eyre::Result<Self> {
        let md_without_start = md
            .strip_prefix("---\n")
            .or_else(|| md.strip_prefix("---\r\n"));
        let stripped = md_without_start
            .and_then(|md| md.split_once("\n---\n"))
            .or_else(|| md_without_start.and_then(|md| md.split_once("\r\n---\r\n")));

        if let Some((front_matter, body)) = stripped {
            let metadata = serde_saphyr::from_str::<TemplateMetadata>(front_matter)?;

            Ok(Self {
                metadata,
                body: body.to_owned(),
            })
        } else {
            Ok(Self {
                metadata: TemplateMetadata::default(),
                body: md.to_owned(),
            })
        }
    }
}

pub async fn get_template_file(
    repo: &RepoName,
    api: &Forgejo,
    name: &str,
) -> eyre::Result<(Vec<u8>, bool)> {
    const DIRS: [&str; 8] = [
        ".forgejo/issue_template",
        ".forgejo/ISSUE_TEMPLATE",
        ".gitea/issue_template",
        ".gitea/ISSUE_TEMPLATE",
        ".github/issue_template",
        ".github/ISSUE_TEMPLATE",
        "docs/issue_template",
        "docs/ISSUE_TEMPLATE",
    ];
    const EXTS: [&str; 3] = ["md", "yml", "yaml"];
    let query = forgejo_api::structs::RepoGetRawFileQuery { r#ref: None };
    for dir in DIRS {
        for ext in EXTS {
            let path = format!("{dir}/{name}.{ext}");
            let file = api
                .repo_get_raw_file(repo.owner(), repo.name(), &path, query.clone())
                .await;
            match file {
                Ok(file) => {
                    let is_yaml = matches!(ext, "yml" | "yaml");
                    return Ok((file.to_vec(), is_yaml));
                }
                Err(forgejo_api::ForgejoError::ApiError(forgejo_api::ApiError {
                    kind: forgejo_api::ApiErrorKind::NotFound { .. },
                    ..
                })) => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    eyre::bail!("Could not find template '{name}'");
}

pub async fn generate_from_template(
    body: Option<String>,
    template_file: Vec<u8>,
    is_yaml: bool,
) -> eyre::Result<(String, TemplateMetadata)> {
    let template_file = std::str::from_utf8(&template_file)?;
    if is_yaml {
        let tmpl =
            serde_saphyr::from_str::<crate::issues::template::yaml::YamlTemplate>(template_file)?;

        if let Some(body) = body {
            return Ok((body, tmpl.metadata));
        }

        let mut form = tmpl.generate_form()?;
        crate::editor(&mut form, Some("md")).await?;
        let body = tmpl.generate_content(tmpl.parse_form(&form)?)?;

        Ok((body, tmpl.metadata))
    } else {
        let mut tmpl = crate::issues::template::MarkdownTemplate::new(template_file)?;

        let body = match body {
            Some(body) => body,
            None => {
                crate::editor(&mut tmpl.body, Some("md")).await?;
                tmpl.body
            }
        };

        Ok((body, tmpl.metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_PR_TEMPLATE: &[u8] = include_bytes!("../../.forgejo/pull_request_template.yml");

    const YAML_TEMPLATE: &str = "\
labels:
  - bug
ref: refs/heads/main
body:
  - type: textarea
    attributes:
      label: Summary
      description: Describe the change.
  - type: input
    attributes:
      label: Version
";

    const MARKDOWN_TEMPLATE: &str = "\
---
labels:
  - bug
ref: refs/heads/main
---

Template placeholder body.
";

    async fn yaml_with_body(body: &str) -> eyre::Result<(String, TemplateMetadata)> {
        generate_from_template(
            Some(body.to_owned()),
            YAML_TEMPLATE.as_bytes().to_vec(),
            true,
        )
        .await
    }

    #[tokio::test]
    async fn yaml_explicit_body_is_returned_verbatim() {
        let body =
            "# Title\n\nSome prose with unicode: äöü ✅\n\n- item\n\n```rust\nfn main() {}\n```\n";
        let (result, _) = yaml_with_body(body).await.unwrap();
        assert_eq!(result, body);
    }

    #[tokio::test]
    async fn yaml_explicit_body_keeps_crlf_and_trailing_newline() {
        let body = "line one\r\nline two\r\n\r\n";
        let (result, _) = yaml_with_body(body).await.unwrap();
        assert_eq!(result, body);
    }

    #[tokio::test]
    async fn yaml_explicit_body_without_trailing_newline() {
        let body = "no trailing newline";
        let (result, _) = yaml_with_body(body).await.unwrap();
        assert_eq!(result, body);
    }

    #[tokio::test]
    async fn yaml_explicit_empty_body_stays_empty() {
        let (result, _) = yaml_with_body("").await.unwrap();
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn yaml_explicit_body_keeps_template_metadata() {
        let (_, metadata) = yaml_with_body("body").await.unwrap();
        assert_eq!(metadata.labels, Some(vec!["bug".to_owned()]));
        assert_eq!(metadata.r#ref, Some("refs/heads/main".to_owned()));
    }

    #[tokio::test]
    async fn yaml_repository_template_accepts_explicit_body() {
        let body = "Plain pull request description.\n";
        let (result, metadata) =
            generate_from_template(Some(body.to_owned()), REPO_PR_TEMPLATE.to_vec(), true)
                .await
                .unwrap();
        assert_eq!(result, body);
        assert_eq!(metadata.labels, None);
        assert_eq!(metadata.r#ref, None);
    }

    #[tokio::test]
    async fn yaml_invalid_template_still_errors_with_explicit_body() {
        let err = generate_from_template(
            Some("body".to_owned()),
            b"labels: [unterminated".to_vec(),
            true,
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(
            !err.to_string().contains("expected header"),
            "template errors must not be masked by the form parser: {err:?}"
        );
    }

    #[tokio::test]
    async fn yaml_invalid_utf8_template_still_errors_with_explicit_body() {
        let err = generate_from_template(
            Some("body".to_owned()),
            vec![0x62, 0x6f, 0x64, 0x79, 0xff, 0xfe],
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("utf-8"), "error: {err:?}");
    }

    #[tokio::test]
    async fn markdown_explicit_body_keeps_frontmatter_metadata() {
        let body = "# Heading\n\ntext\n";
        let (result, metadata) = generate_from_template(
            Some(body.to_owned()),
            MARKDOWN_TEMPLATE.as_bytes().to_vec(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(result, body);
        assert_eq!(metadata.labels, Some(vec!["bug".to_owned()]));
        assert_eq!(metadata.r#ref, Some("refs/heads/main".to_owned()));
    }

    #[test]
    fn yaml_form_roundtrip_still_works() {
        let tmpl: yaml::YamlTemplate = serde_saphyr::from_str(YAML_TEMPLATE).unwrap();
        let form = tmpl.generate_form().unwrap();
        let parsed = tmpl.parse_form(&form).unwrap();
        let content = tmpl.generate_content(parsed).unwrap();
        assert!(content.contains("Summary"), "content: {content}");
        assert!(content.contains("Version"), "content: {content}");
    }

    #[test]
    fn yaml_form_parser_still_rejects_malformed_forms() {
        let tmpl: yaml::YamlTemplate = serde_saphyr::from_str(YAML_TEMPLATE).unwrap();
        let err = tmpl
            .parse_form("not a form at all\n")
            .err()
            .expect("malformed form must be rejected");
        assert!(
            format!("{err:?}").contains("expected header"),
            "error: {err:?}"
        );
    }
}
