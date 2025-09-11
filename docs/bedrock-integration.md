# Amazon Bedrock Integration

This document explains how to configure and use Amazon Bedrock with omni-dev for Claude API access through AWS.

## Overview

omni-dev supports using Claude through Amazon Bedrock as an alternative to the direct Anthropic API. This integration allows you to:

- Use AWS IAM permissions and billing
- Access Claude models through your AWS account
- Leverage AWS credential management
- Use multiple authentication methods (profiles, SSO, environment variables, etc.)

## Configuration

### 1. AWS Setup

First, ensure you have access to Claude models in Amazon Bedrock:

1. **Enable Model Access**: Go to the AWS Bedrock console and request access to Anthropic Claude models
2. **IAM Permissions**: Ensure your AWS credentials have the following permissions:
   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Action": [
           "bedrock:InvokeModel",
           "bedrock:InvokeModelWithResponseStream",
           "bedrock:ListInferenceProfiles"
         ],
         "Resource": "arn:aws:bedrock:*::foundation-model/anthropic.*"
       }
     ]
   }
   ```

### 2. Claude Code Settings

Create or update your `~/.claude/settings.json` file:

#### Basic Configuration

```json
{
  "defaultProvider": "bedrock",
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "profile",
    "profile": "default",
    "models": {
      "claude": "anthropic.claude-3-5-sonnet-20241022-v2:0",
      "default": "anthropic.claude-3-5-sonnet-20241022-v2:0"
    }
  }
}
```

#### Advanced Configuration

```json
{
  "defaultProvider": "bedrock",
  "anthropic": {
    "defaultModel": "claude-3-5-sonnet-20241022"
  },
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "sso",
    "ssoProfile": "my-sso-profile",
    "models": {
      "claude": "anthropic.claude-3-5-sonnet-20241022-v2:0",
      "default": "anthropic.claude-3-5-sonnet-20241022-v2:0"
    },
    "auth": {
      "credentialExport": {
        "enabled": true,
        "command": "aws configure export-credentials --profile my-profile --format env"
      }
    }
  },
  "apiKeyHelper": {
    "enabled": false
  }
}
```

### 3. Authentication Methods

#### Profile-based (Recommended)

```json
{
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "profile",
    "profile": "my-aws-profile"
  }
}
```

#### Environment Variables

```json
{
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "environment"
  }
}
```

Set these environment variables:
```bash
export AWS_ACCESS_KEY_ID="your-access-key"
export AWS_SECRET_ACCESS_KEY="your-secret-key"
export AWS_REGION="us-east-1"
```

#### AWS SSO

```json
{
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "sso",
    "ssoProfile": "my-sso-profile"
  }
}
```

#### IAM Role (for EC2/Lambda)

```json
{
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "iam-role"
  }
}
```

## Available Models

The following Claude models are available through Bedrock (as of 2024):

- `anthropic.claude-3-5-sonnet-20241022-v2:0` (recommended)
- `anthropic.claude-3-sonnet-20240229-v1:0`
- `anthropic.claude-3-haiku-20240307-v1:0`
- `anthropic.claude-v2:1`
- `anthropic.claude-v2`

Check the AWS Bedrock console for the most up-to-date list of available models in your region.

## Usage

Once configured, omni-dev will automatically use Bedrock when:

1. `bedrock.enabled` is `true` in settings.json
2. `defaultProvider` is set to `"bedrock"` (optional, will be auto-detected)

All existing omni-dev commands work unchanged:

```bash
# Generate commit message amendments
omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context

# View commit analysis
omni-dev git commit message view 'HEAD^..HEAD'

# Create pull requests with improved messages
omni-dev pr create
```

## Fallback Behavior

If Bedrock is configured but fails (due to permissions, network issues, etc.), omni-dev will automatically fall back to the Anthropic API if available.

To disable fallback and use Bedrock strictly:

```bash
# Set environment variable
export OMNI_DEV_STRICT_PROVIDER=true
```

## Troubleshooting

### Common Issues

#### "No model access" Error

```
Error: Bedrock API error: AccessDeniedException
```

**Solution**: Request model access in the AWS Bedrock console for Anthropic Claude models.

#### "Invalid region" Error

```
Error: The model ID is not supported in this region
```

**Solution**: Check model availability in your region and update the `region` in settings.json.

#### Authentication Errors

```
Error: Configuration error: AWS credentials not found
```

**Solutions**:
1. Verify your AWS credentials: `aws sts get-caller-identity`
2. Check your profile configuration: `aws configure list --profile your-profile`
3. For SSO: Run `aws sso login --profile your-sso-profile`

### Debug Mode

Enable debug logging to troubleshoot issues:

```bash
export RUST_LOG=omni_dev=debug
omni-dev git commit message view HEAD --use-context
```

### Checking Configuration

Verify your configuration is loaded correctly:

```bash
# This will show which provider is being used
omni-dev git commit message view HEAD --use-context
```

## Cost Considerations

- **Bedrock Pricing**: Pay per API call through your AWS account
- **Anthropic API**: Pay per API call through Anthropic directly
- **Rate Limits**: Bedrock has its own rate limits separate from Anthropic API

Monitor usage through:
- AWS CloudWatch metrics
- AWS Cost Explorer
- Bedrock console usage dashboard

## Security Best Practices

1. **Use IAM roles** when possible instead of access keys
2. **Rotate credentials** regularly
3. **Use least-privilege permissions** (only the required Bedrock permissions)
4. **Enable CloudTrail logging** for Bedrock API calls
5. **Use SSO profiles** for multi-account environments

## Regional Availability

Claude models are available in select AWS regions. Check the current availability:

- US East (N. Virginia): `us-east-1`
- US West (Oregon): `us-west-2`
- Europe (Ireland): `eu-west-1`
- Asia Pacific (Sydney): `ap-southeast-2`
- Asia Pacific (Tokyo): `ap-northeast-1`

Refer to the [AWS Bedrock documentation](https://docs.aws.amazon.com/bedrock/latest/userguide/models-supported.html) for the most current regional availability.

## Migration from Anthropic API

To migrate from direct Anthropic API to Bedrock:

1. **Keep existing settings**: Your `anthropic` configuration remains as fallback
2. **Add Bedrock configuration**: Add the `bedrock` section to settings.json
3. **Test gradually**: Start with non-critical commits to verify everything works
4. **Monitor costs**: Compare pricing between direct API and Bedrock usage

Example migration configuration:

```json
{
  "defaultProvider": "bedrock",
  "anthropic": {
    "apiKey": "sk-ant-...",
    "defaultModel": "claude-3-5-sonnet-20241022"
  },
  "bedrock": {
    "enabled": true,
    "region": "us-east-1",
    "authMethod": "profile",
    "profile": "default",
    "models": {
      "claude": "anthropic.claude-3-5-sonnet-20241022-v2:0"
    }
  }
}
```

This configuration will use Bedrock by default but fall back to Anthropic API if Bedrock fails.