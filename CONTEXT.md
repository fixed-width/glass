# Glass desktop language

Glass prepares a place for application work, then exposes its normal application tools to an agent.

## Language

**Application setup…**: The user-facing place to choose where apps run and complete any required setup.
_Avoid_: desktop enrollment, provisioning, session configuration in ordinary UI.

**On your desktop**: Applications and input use the user's visible desktop. Input can change their pointer and focus.
_Avoid_: shared mode, main-session mode in ordinary UI.

**Off your desktop…**: Applications and input use a separate desktop context after the user opts in.
_Avoid_: sandbox mode; containment is a separate choice.

**Environment**: The prepared desktop context used for application work. It can remain available between application runs.
_Avoid_: application session when referring to the retained desktop.

**Start Environment…**: The user's action to prepare an off-desktop environment and complete required authentication.
_Avoid_: Start development desktop, launch child session in ordinary UI.

**Stop Environment**: End the owned off-desktop environment and its application work while leaving Glass running.
_Avoid_: Stop app when ending the environment.

**Ready**: The selected desktop context is verified for application work; configuration alone is insufficient.

**Quit Glass**: Stop the resident Glass application and its owned application work.
_Avoid_: Close when a window merely hides.

**Containment**: The selected application's file, network and clipboard restrictions, independent of its desktop choice.

**Menu bar / system tray**: The macOS and Windows locations for the resident application, respectively.

**Log In / sign in**: The native macOS and Windows terms for account authentication, respectively.

**Start with Windows**: The Windows preference to open Glass automatically when the user signs in.
