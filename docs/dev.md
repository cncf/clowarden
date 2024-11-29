# Development environment setup

This document will help you setup your development environment so that you can build, test and run CLOWarden locally from source.

The instructions provided in this document rely on a set of [aliases](#aliases) available at the end. These aliases are used by some of the maintainers and are provided only as examples. Please feel free to adapt them to suit your needs. You may want to add them to your shell's configuration file so that they are loaded automatically.

To start, please clone the [CLOWarden repository](https://github.com/cncf/clowarden). If you plan to use the aliases mentioned above, you should set the `CLOWARDEN_SOURCE` variable to the path where you cloned the repository.

## Database

The datastore used by CLOWarden is PostgreSQL. You can install it locally using your favorite OS package manager.

Once PostgreSQL is installed and its binaries are available in your `PATH`, we can initialize the database cluster and launch the database server:

```sh
clowarden_db_init
clowarden_db_server
```

Once the database server is up an running, we can create the `clowarden` database and we'll be ready to go:

```sh
clowarden_db_create
```

### Migrations

[Database migrations](https://github.com/cncf/clowarden/tree/main/database/migrations) are managed using [Tern](https://github.com/jackc/tern). Please [install it](https://github.com/jackc/tern#installation) before proceeding. The database schema and functions are managed with migrations.

We need to create a configuration file so that Tern knows how to connect to our database. We'll create a file called `tern.conf` inside `~/.config/clowarden` with the following content (please adjust as needed):

```ini
[database]
host = localhost
port = 5432
database = clowarden
user = postgres
```

Now that the `clowarden` database server is up and ready, we just need to apply all available migrations using the following command:

```sh
clowarden_db_migrate
```

## Backend

The backend is written in [Rust](https://www.rust-lang.org), and it can be built from the source by using [Cargo](https://rustup.rs), the Rust package manager.

To build the backend components, please run the command below:

```sh
cargo build
```

Even if you don't plan to do any work on the frontend side of CLOWarden, you may be interested in building it once if you would like to use the CLOWarden web audit tool. To do this, you will have to install [yarn](https://yarnpkg.com/getting-started/install). Once you have it installed, you can build the frontend application by running the following commands:

```sh
cd web && yarn install
clowarden_frontend_build
```

### CLOWarden server

The CLOWarden server is in charge of serving the webhooks endpoints to receive events from external services (i.e. GitHub), as well as the audit tool static assets and the endpoints to power it.

Once you have a working Rust development environment set up and the audit tool built, it's time to launch the `server`. Before running it, we'll need to create a configuration file in `~/.config/clowarden` named `clowarden.yaml` with the following content (please adjust as needed):

> [!NOTE]
> To test CLOWarden locally, you'll need to setup a GitHub application. We recommend creating a new GitHub application for testing purposes. For more details about how to create the application and the permissions needed, please see the chart's [README](https://artifacthub.io/packages/helm/clowarden/clowarden) file. Keep in mind that in the GitHub application settings you'll need to specify the webhook URL, so you'll need to expose your local server to the Internet. You can use a service like Cloudflare's tunnel or Ngrok to achieve this.

```yaml
db:
  host: localhost
  port: 5432
  dbname: clowarden
  user: postgres
  password: ""

server:
  addr: 0.0.0.0:9000
  staticPath: /<YOUR_CLOWARDEN_LOCAL_PATH>/web/build
  githubApp:
    # Replace with the information from your dev GitHub application

    # GitHub application ID
    appId: <YOUR_GITHUB_APP_ID>

    # GitHub application private key
    privateKey: |-
      -----BEGIN RSA PRIVATE KEY-----
      ...
      <YOUR_APP_PRIVATE_KEY>
      ...
      -----END RSA PRIVATE KEY-----

    # GitHub application webhook secret
    webhookSecret: <YOUR_WEBHOOK_SECRET>

services:
  github:
    enabled: true

organizations:
  - name: <GITHUB_ORG_TO_MANAGE_WITH_CLOWARDEN>
    installationId: <GITHUB_APP_INSTALLATION_ID>
    repository: <GITHUB_REPO_CONTAINING_CLOWARDEN_CONFIG>
    branch: main
    legacy:
      enabled: true
      sheriffPermissionsPath: config.yaml
```

Now you can run the `server`:

```sh
clowarden_server
```

Once it is up and running, CLOWarden will be ready to process events from GitHub. You can give it a try by creating a new pull request to update the permissions file. If you point your browser to <http://localhost:9000/audit>, you should see the CLOWarden audit tool.

### CLI

CLOWarden includes a CLI tool that can be handy when adding new organizations to your CLOWarden deployment.

You can use it to:

- Validate the configuration in the repository provided
- Display changes between the actual state and the desired state
- Generate a configuration file from the actual state

> [!NOTE]
> This tool uses the GitHub API, which requires authentication. Please make sure you provide a GitHub token (with repo and read:org scopes) by setting the GITHUB_TOKEN environment variable.

If you are using the aliases provided below, you can run it this way:

```sh
export GITHUB_TOKEN=<your token>

clowarden_cli --help
```

### Backend tests

You can run the backend tests by using `cargo` as well. Just run the following command:

```sh
cargo test
```

## Frontend

CLOWarden includes a web based audit tool that allows maintainers to easily search and inspect applied changes. This audit tool is a single page application written in [TypeScript](https://www.typescriptlang.org) using [React](https://reactjs.org).

In the backend section we mentioned how to install the frontend dependencies and build it. That should be enough if you are only going to work on the backend. However, if you are planning to do some work on the frontend, it may be better to launch an additional server which will rebuild the web application as needed whenever a file is modified.

The frontend development server can be launched using the following command:

```sh
clowarden_frontend_dev
```

That alias will launch an http server that will listen on the port `3000`. Once it's running, you can point your browser to [http://localhost:3000](http://localhost:3000) and you should see the CLOWarden audit tool. The page will be automatically reloaded everytime you make a change in the frontend code. Build errors and build warnings will be visible in the console.

API calls will go to <http://localhost:9000>, so the [CLOWarden server](#clowarden-server) is expected to be up and running.

## Aliases

The following aliases are used by some of the maintainers and are provided only as examples. Please feel free to adapt them to suit your needs.

```sh
export CLOWARDEN_SOURCE=~/projects/clowarden
export CLOWARDEN_DATA=~/tmp/data_clowarden

alias clowarden_db_init="mkdir -p $CLOWARDEN_DATA && initdb -U postgres $CLOWARDEN_DATA"
alias clowarden_db_create="psql -U postgres -c 'create database clowarden'"
alias clowarden_db_drop="psql -U postgres -c 'drop database clowarden with (force)'"
alias clowarden_db_recreate="clowarden_db_drop && clowarden_db_create && clowarden_db_migrate"
alias clowarden_db_server="postgres -D $CLOWARDEN_DATA"
alias clowarden_db_client="psql -h localhost -U postgres clowarden"
alias clowarden_db_migrate="pushd $CLOWARDEN_SOURCE/database/migrations; TERN_CONF=~/.config/clowarden/tern.conf ./migrate.sh; popd"
alias clowarden_server="$CLOWARDEN_SOURCE/target/debug/clowarden-server -c ~/.config/clowarden/clowarden.yaml"
alias clowarden_cli="$CLOWARDEN_SOURCE/target/debug/clowarden-cli"
alias clowarden_frontend_build="pushd $CLOWARDEN_SOURCE/web; yarn build; popd"
alias clowarden_frontend_dev="pushd $CLOWARDEN_SOURCE/web; yarn start; popd"
```
