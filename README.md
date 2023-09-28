# CLOWarden

**CLOWarden** is a tool that manages the access to resources across multiple services with the initial focus on repositories in a GitHub organization.
CLOWarden allows you to grant access to an individual user or a defined team of users by submitting a PR to a file that defines access rules.

The CNCF initially used [Sheriff](https://github.com/cncf/sheriff) to manage access to resources. CLOWarden has replaced Sheriff with a system that suits better the needs of the CNCF.

CLOWarden supports a legacy configuration mode that allows using [a subset of the Sheriff's permissions configuration file](#configuration) and the [CNCF's people file](https://github.com/cncf/people/blob/main/people.json) to define users, teams and GitHub repositories (the same files currently used by CNCF at <https://github.com/cncf/people>).

## How it works

CLOWarden's main goal is to ensure that the resources' **desired state**, as defined in the configuration files, matches the **actual state** in the corresponding services. This is achieved by running *reconciliation* jobs, that can be triggered *on-demand* or *periodically*. These reconciliation jobs are in charge of applying the necessary changes to address the differences between the desired and actual state on each service, which is done by delegating some work on specialized *service handlers*.

CLOWarden monitors pull requests created in the configuration repository and, when applicable, it validates the proposed changes to access rules and creates reconciliation jobs to apply the necessary changes. This is what we call an on-demand reconciliation job: it's created as a result of a user's submitted pull request, and changes are applied *immediately* once the pull request is approved and merged.

![reconciliation-completed-success](docs/screenshots/reconciliation-completed-success.png)

Sometimes, however, this may not be enough. Changes can be applied manually to the service bypassing the configuration files (i.e. from the GitHub settings UI), and CLOWarden still needs to make sure that the actual state matches the desired state documented in the configuration files. So in addition to on-demand reconciliation jobs, CLOWarden runs *periodic* ones to ensure everything is all right all the time.

### State

The core piece of state in CLOWarden is the **directory**, which is a catalog that contains **teams** and **users**. The directory is at the disposal of all the services handlers, allowing them to take the appropriate action for each directory change detected. For example, when a new team is added to the directory, the GitHub service handler will create that team on the GitHub organization.

But teams and users may not be enough in some cases, and some service handlers may need to define additional resources. This is the case of the GitHub service handler, which defines an additional resource, the repository.

## Sample workflow

Changes to resources in services managed by CLOWarden should be proposed via *pull requests*. CLOWarden will check all pull requests created in the configuration repository defined and, when it detects that the PR contains changes to any of the configuration files, it will start working on it.

Let's go through a full example to see how this would work in practice.

Our goal in this example will be to create a new team (named `team1`) with one maintainer and one member, as well as a new repository (named `repo1`). We want to give `team1` write permissions on `repo1`, and we'd also like to add a external collaborator, named `collaborator1`, with read permissions.

The first step will be to create a pull request to add the entries below to the configuration files

(*This configuration intentionally introduces a typo so we can describe CLOWarden's PR validation checks -team1 is misspelled-*):

```yaml
teams:
  - name: tem1                  # This is a deliberate typo. The value should be "team1" not "tem1"
    maintainers:
      - maintainer1
    members:
      - member1

...

repositories:
  - name: repo1
    teams:
      team1: write              # team1 does not exist! The CLOWarden validation check will report an error in a PR as a comment
    external_collaborators:
      collaborator1: read
    visibility: public
```

As soon as the pull request is created, CLOWarden **validates** the changes proposed.

One of the goals of CLOWarden is to make it *as simple as possible for maintainers to review and approve suggested changes* to the configuration. To do that, CLOWarden provides feedback in pull requests in the form of comments. Suggested changes can be invalid for a number of reasons, like a syntax problem in the configuration file, or not following any of the rules, like using an invalid role when defining permissions. CLOWarden tries its best to give helpful feedback to the pull request creator, to point them in the right direction and help address errors without requiring the maintainers intervention.

In this case, the error we introduced intentionally was caught: we incorrectly defined the new team as `tem1`, but then try to reference it as `team1` in the repository access definition.

![validation-error](docs/screenshots/validation-error.png)

If this error had not been caught at validation time, a team named `tem1` would have been created and the process of granting permissions to `team1` on `repo1` would have failed as `team1` wouldn't have existed.

Please note that, in addition to the feedback comment, CLOWarden created a **check** in the PR to indicate that the configuration changes are not valid. When used in combination with branch protection, this can help prevent that invalid configuration changes are merged.

![invalid-config-check-run](docs/screenshots/invalid-config-check-run.png)

The pull request creator can now push a fix to address these issues. Once that's done, CLOWarden will validate the changes again automatically.

![validation-succeeded](docs/screenshots/validation-succeeded.png)

Now CLOWarden is happy with the changes proposed! This time, it also tried to help the maintainer who will approve the changes by describing in the comment what had changed.

Sometimes this may be easy to spot by just looking at a the diff on the PR. But on other occasions, depending on the changes applied, it can get trickier and be error prone, as just a single extra space or tabulation can have unintented consequences. So CLOWarden simplifies this by analyzing the changes itself and displaying them in an easy to read way as a comment on the PR.

Outside of the context of a PR it is possible to view an autdit log of the changes made see the [#Audit tool](Audit tool) below

![valid-config-check-run](docs/screenshots/valid-config-check-run.png)

Now that the changes are valid, the check has been updated to reflect the new status and the PR can be merged safely once the maintainers are ready. As soon as this happens, CLOWarden will create a reconciliation job and will apply the necessary changes so that the actual state in the services matches the new desired state defined in the configuration. Once the job is executed, a new comment will be posted on the PR with more details:

![reconciliation-completed-success](docs/screenshots/reconciliation-completed-success.png)

In this case all changes were applied successfully, but if something would have gone wrong the comment would display the corresponding error.

## Audit tool

CLOWarden registers all changes applied to the services in a database. Even though most of the time all information related to a given change will be accessible on the PR that triggered it, sometimes it may be necessary to go a bit further to answer questions like:

- *When* was user1 *granted access* to repository1 and *who approved granting that access*?
- In what PR was *team1 removed?*
- What changes have been applied *by automatic periodic reconciliations* during the *last 24 hours?*

To help to answer these questions quickly, CLOWarden provides an audit tool that allows maintainers to easily search and inspect applied changes. The audit tool can be accessed by using a web browser and is available at: `https://YOUR-CLOWARDEN-URL/audit/`.

![audit-tool](docs/screenshots/audit-tool.png)

## Services supported

The following services are supported at the moment:

### GitHub

Operations supported:

- Add teams
- Remove teams
- Add maintainers or members to teams
- Remove maintainers or members from teams
- Add repositories
- Add teams to repositories
- Remove teams from repositories
- Update teams' role in repository
- Add collaborators to repositories
- Remove collaborators from repositories
- Update collaborators' role in repository
- Update repository visibility

## Configuration

CLOWarden supports a legacy configuration mode that allows using a subset of the Sheriff's permissions configuration file.

```yaml
teams:
  - name: <github_team_slug>
    # Team maintainers
    #
    #   - Values must be valid GitHub usernames (case sensitive)
    #   - At least one team maintainer must be specified
    #   - Maintainers must already be members of the organization
    #   - Can be omitted if at least one maintainer is defined via formation
    maintainers:
      - <github_username>
      - <github_username>

    # Team members
    #
    #   - Values must be valid GitHub usernames (case sensitive)
    members:
      - <github_username>
      - <github_username>

    # Formation allows populating teams' maintainers and members from the
    # content of other teams
    #
    #  - Each value must be a valid GitHub team slug
    #  - Formation is not recursive
    #  - This field can be used in combination with maintainers and members
    formation:
      - <github_team_slug>
      - <github_team_slug>

repositories:
  - name: <github_repository_name>
    # Teams with access to the repository
    #
    #   - Key: GitHub team slug
    #   - Value: access level
    #   - Value options: read | triage | write | maintain | admin
    teams:
      <github_team_slug>: maintain
      <github_team_slug>: write

    # External collaborators with access to the repository
    #
    #   - Key: GitHub username (case sensitive)
    #   - Value: access level
    #   - Value options: read | triage | write | maintain | admin
    external_collaborators:
      <github_username>: write
      <github_username>: read

    # Repository visibility
    #
    #   - Value options: public | private | internal
    #   - Default: public
    visibility: public
```

It's important to keep in mind that..

- GitHub usernames are case sensitive
- Repositories and team names must contain only lowercase letters, numbers or hyphens (in the case of teams, the GitHub team slug must be used)
- Teams maintainers must belong to the organization before being added to the teams
- Teams maintainers and members fields can be omitted when the field formation is defined and one of the subteams has at least one maintainer
- It is possible to use the formation field in teams and at the same time explicitly define some team maintainers and members
- Teams formation is not recursive. If a subteam is also using formation, its subteams will be ignored
- GitHub repositories permissions granted using teams won't be effective until the team member has accepted the invitation to the organization

## Using CLOWarden in your organization

Although has been deployed for use in the CNCF GitHub org, CLOWarden is still in an experimental phase and breaking changes are expected, so we do not recommend its use in other production enviroments yet. Once it stabilizes, we'll publish some additional documentation to make it easier to run your own CLOWarden instance.

## Contributing

Please see [CONTRIBUTING.md](./CONTRIBUTING.md) for more details.

## Code of Conduct

This project follows the [CNCF Code of Conduct](https://github.com/cncf/foundation/blob/master/code-of-conduct.md).

## License

CLOWarden is an Open Source project licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0).
