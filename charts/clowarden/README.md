# CLOWarden

[CLOWarden](https://clowarden.io) is a tool that manages access to resources across multiple services.

## Introduction

This chart bootstraps a CLOWarden deployment on a [Kubernetes](http://kubernetes.io) cluster using the [Helm](https://helm.sh) package manager.

## Prerequisites

Before installing this chart, you need to [setup a GitHub application](https://docs.github.com/en/apps/creating-github-apps/creating-github-apps/creating-a-github-app). The application requires the following permissions [to be set](https://docs.github.com/en/apps/maintaining-github-apps/editing-a-github-apps-permissions):

Repository:

- **Administration**: *read/write*
- **Checks**: *read/write*
- **Contents**: *read*
- **Metadata**: *read*
- **Pull requests**: *read/write*

Organization:

- **Administration**: *read/write*
- **Members**: *read/write*

In addition to those permissions, it must also be subscribed to the following events:

- *Pull Request*

CLOWarden expects GitHub events to be sent to the `/webhook/github` endpoint. In the GitHub application, please enable `webhook` and set the target URL to your exposed endpoint (ie: <https://your-clowarden-deployment/webhook/github>). You will need to define a random secret for the webhook (you can use the following command to do it: `openssl rand -hex 32`). Please note your webhook secret, as well as the GitHub application ID and private key, as you'll need them in the next step when installing the chart.

Once your GitHub application is ready you can install it in the organizations you need.

## Installing the chart

Create a values file (`my-values.yaml`) that includes the configuration values required for your GitHub application:

```yaml
server:
  githubApp:
    # GitHub application ID
    appId: 123456 # Replace with your GitHub app ID

    # GitHub application private key
    privateKey: |-
      -----BEGIN RSA PRIVATE KEY-----
      ...
      YOUR_APP_PRIVATE_KEY
      ...
      -----END RSA PRIVATE KEY-----

    # GitHub application webhook secret
    webhookSecret: "your-webhook-secret"

    # GitHub application webhook secret fallback (handy for webhook secret rotation)
    webhookSecretFallback: "old-webhook-secret"
```

In addition to the GitHub application configuration, you can also add the organizations you'd like to use CLOWarden with at this point:

```yaml
organizations:
  - # Name of the GitHub organization
    name: org-name

    # CLOWarden's GitHub application installation id
    installationId: 12345678

    # Repository where the configuration files are located
    repository: .clowarden

    # Branch to use in the configuration repository
    branch: main

    # Legacy mode configuration
    legacy:
      # Whether legacy mode is enabled or not (must be at the moment)
      enabled: true

      # Path of the Sheriff's permissions file
      sheriffPermissionsPath: config.yaml
```

CLOWarden includes a CLI tool that can be handy when adding new organizations to your CLOWarden deployment. For more information please see the [repository's README file](https://github.com/cncf/clowarden?#cli-tool).

To install the chart with the release name `my-clowarden` run:

```bash
$ helm repo add clowarden https://cncf.github.io/clowarden/
$ helm install --values my-values.yaml my-clowarden clowarden/clowarden
```

The command above deploys CLOWarden on the Kubernetes cluster using the default configuration values and the GitHub application configuration provided. Please see the [chart's default values file](https://github.com/cncf/clowarden/blob/main/charts/clowarden/values.yaml) for a list of all the configurable parameters of the chart and their default values.

## Uninstalling the chart

To uninstall the `my-clowarden` deployment run:

```bash
$ helm uninstall my-clowarden
```

This command removes all the Kubernetes components associated with the chart and deletes the release.

## How CLOWarden works

For more information about how CLOWarden works from a user's perspective please see the [repository's README file](https://github.com/cncf/clowarden#readme).
