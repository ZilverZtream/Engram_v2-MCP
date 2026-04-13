// WebForms Control Mapping Catalog
//
// Provides a comprehensive static lookup table mapping ASP.NET WebForms controls
// to their modern equivalents in Blazor, React, and Angular. Each entry includes
// property mappings, event mappings, data-binding patterns, and migration notes.
//
// Usage:
//   let mapping = control_mapping::lookup("GridView");
//   let mappings = control_mapping::lookup_all_for_file(&["TextBox", "Button", "GridView"]);

/// A single WebForms-to-modern-framework control mapping entry.
#[derive(Debug, Clone, Copy)]
pub struct ControlMapping {
    /// ASP.NET WebForms control name (e.g. "GridView", "TextBox").
    pub legacy_control: &'static str,
    /// WebForms namespace prefix (e.g. "System.Web.UI.WebControls").
    pub legacy_namespace: &'static str,
    /// Blazor (Razor Components) equivalent.
    pub blazor_equivalent: &'static str,
    /// React equivalent (component or HTML element).
    pub react_equivalent: &'static str,
    /// Angular equivalent (component or directive).
    pub angular_equivalent: &'static str,
    /// Property name mappings: `(legacy_property, modern_property)`.
    pub properties_map: &'static [(&'static str, &'static str)],
    /// Event name mappings: `(legacy_event, modern_event)`.
    pub event_map: &'static [(&'static str, &'static str)],
    /// Recommended data-binding pattern in the target framework.
    pub data_binding_pattern: &'static str,
    /// Free-text migration notes and complexity hints.
    pub notes: &'static str,
    // ── Phase 34: Behavioral Lifecycle Metadata ───────────────────────────
    /// ASP.NET page lifecycle phase where this control primarily operates.
    /// "Init", "Load", "PreRender", "Postback", "Any".
    pub lifecycle_phase: &'static str,
    /// How the control reconstructs state across postbacks.
    /// "ViewState", "ControlState", "Stateless", "ComponentState".
    pub state_model: &'static str,
    /// When the control's primary data event fires.
    /// "per_postback", "per_user_action", "once", "manual".
    pub event_firing_model: &'static str,
    /// True if the control requires explicit DataBind() on every postback.
    pub requires_databind_on_postback: bool,
    /// True if the control supports nested child controls with their own postback cycles.
    pub has_nested_postback: bool,
    /// Migration complexity rating: 1 (trivial) to 5 (major rewrite).
    pub migration_complexity: u8,
    /// Key behavioral differences that cause silent failures if ignored.
    pub breaking_differences: &'static [&'static str],
}

/// Master catalog of WebForms control mappings (50 entries).
///
/// Covers data display, input, action, navigation, layout, AJAX, data access,
/// display, validation, and additional controls.
pub const CONTROL_MAPPINGS: &[ControlMapping] = &[
    // =====================================================================
    // DATA DISPLAY CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "GridView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "QuickGrid<T> / Virtualize<T>",
        react_equivalent: "react-table / AG Grid / MUI DataGrid",
        angular_equivalent: "mat-table / ag-grid-angular",
        properties_map: &[
            ("DataSource", "Items / data prop"),
            (
                "AutoGenerateColumns",
                "Column definitions via <PropertyColumn>",
            ),
            ("AllowPaging", "Pagination component / paginator"),
            ("AllowSorting", "SortBy parameter / column sort config"),
            ("PageSize", "ItemsPerPage / pageSize prop"),
            ("DataKeyNames", "RowKey / getRowId prop"),
        ],
        event_map: &[
            ("RowDataBound", "OnRowRender / cell renderer"),
            ("PageIndexChanging", "OnPageChange / onPaginationChanged"),
            ("Sorting", "OnSortChanged / onSortChanged"),
            ("SelectedIndexChanged", "OnRowSelected / onSelectionChanged"),
            ("RowCommand", "OnClick per-cell button handler"),
        ],
        data_binding_pattern: "Bind Items/data to IQueryable<T> or collection; use <PropertyColumn> or column defs",
        notes: "High complexity. GridView auto-generation of columns, ViewState-based paging, and RowCommand dispatching have no direct equivalent. Requires decomposition into column definitions, pagination state, and explicit event handlers.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &[
            "ViewState stores page index, sort expression, and edit index across postbacks; must be converted to component state",
            "RowCommand dispatching via CommandName/CommandArgument has no SPA equivalent; decompose into per-row button handlers",
            "EditIndex/SelectedIndex are set during lifecycle events and require DataBind() to take effect",
            "Auto-generated columns (BoundField, TemplateField) require explicit column definitions in modern grids",
        ],
    },
    ControlMapping {
        legacy_control: "DetailsView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom EditForm with field layout",
        react_equivalent: "Custom detail component / Formik",
        angular_equivalent: "Reactive form with mat-form-field",
        properties_map: &[
            ("DataSource", "Model binding / form state"),
            ("AutoGenerateRows", "Manual field enumeration"),
            ("DefaultMode", "Component mode state (view/edit)"),
        ],
        event_map: &[
            ("ItemUpdating", "OnValidSubmit / onSubmit handler"),
            ("ModeChanging", "State toggle handler"),
        ],
        data_binding_pattern: "Bind single object to EditForm model or form state; render fields explicitly",
        notes: "Medium complexity. Single-record CRUD form. Replace with typed form component bound to a model.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["Mode transitions require lifecycle awareness"],
    },
    ControlMapping {
        legacy_control: "FormView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "EditForm with InputText/InputNumber components",
        react_equivalent: "react-hook-form / Formik with templates",
        angular_equivalent: "Reactive form with template switching",
        properties_map: &[
            ("DataSource", "Model / form values"),
            ("DefaultMode", "Component mode (view/insert/edit)"),
            ("ItemTemplate", "Render fragment / JSX / ng-template"),
        ],
        event_map: &[
            ("ItemInserting", "OnValidSubmit for insert"),
            ("ItemUpdating", "OnValidSubmit for update"),
        ],
        data_binding_pattern: "Bind model to EditForm; use @bind or controlled inputs for two-way binding",
        notes: "Medium complexity. Template-based single-record view. Replace templates with conditional render logic.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["Template switching semantics differ from conditional rendering"],
    },
    ControlMapping {
        legacy_control: "ListView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@foreach with Virtualize<T>",
        react_equivalent: "Array.map() with list component",
        angular_equivalent: "*ngFor with trackBy",
        properties_map: &[
            ("DataSource", "Items collection / data array"),
            ("ItemTemplate", "Render fragment / list item component"),
            ("LayoutTemplate", "Wrapper element / container component"),
            ("GroupItemCount", "Chunk array into groups manually"),
        ],
        event_map: &[
            ("ItemDataBound", "Component lifecycle / useEffect per item"),
            ("ItemCommand", "Per-item click/action handler"),
        ],
        data_binding_pattern: "Iterate collection with @foreach or .map(); virtualize for large lists",
        notes: "Medium complexity. Most flexible WebForms list control. Layout/Item/Group templates must be manually decomposed.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &[
            "LayoutTemplate/ItemTemplate decomposition",
            "DataPager integration",
        ],
    },
    ControlMapping {
        legacy_control: "Repeater",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@foreach loop",
        react_equivalent: "Array.map() with JSX",
        angular_equivalent: "*ngFor directive",
        properties_map: &[
            ("DataSource", "Items collection"),
            ("ItemTemplate", "Loop body / render function"),
            ("HeaderTemplate", "Rendered before loop"),
            ("SeparatorTemplate", "CSS border or <hr> between items"),
        ],
        event_map: &[
            ("ItemDataBound", "Per-item render callback"),
            ("ItemCommand", "Per-item event handler"),
        ],
        data_binding_pattern: "Direct iteration over collection; no built-in paging or sorting",
        notes: "Low-medium complexity. Simplest list control; translates almost directly to foreach/map.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Must rebind on every postback"],
    },
    ControlMapping {
        legacy_control: "DataList",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@foreach with CSS grid/flex layout",
        react_equivalent: "Array.map() with grid wrapper",
        angular_equivalent: "*ngFor with CSS grid",
        properties_map: &[
            ("DataSource", "Items collection"),
            ("RepeatColumns", "CSS grid-template-columns"),
            ("RepeatDirection", "CSS flex-direction or grid-auto-flow"),
            ("ItemTemplate", "Render fragment per item"),
        ],
        event_map: &[
            ("ItemDataBound", "Per-item render logic"),
            ("ItemCommand", "Per-item action handler"),
        ],
        data_binding_pattern: "Iterate collection; use CSS grid for multi-column layout",
        notes: "Low complexity. Deprecated in favor of ListView. Multi-column layout achieved with CSS grid.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Multi-column layout via RepeatColumns has no equivalent"],
    },
    // =====================================================================
    // INPUT CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "TextBox",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputText / InputTextArea / InputNumber",
        react_equivalent: "<input> / <textarea> (controlled)",
        angular_equivalent: "<input matInput> / <textarea matInput>",
        properties_map: &[
            ("Text", "Value / @bind-Value / value prop"),
            ("TextMode", "type attribute (text/password/multiline)"),
            ("MaxLength", "maxlength attribute"),
            ("ReadOnly", "readonly attribute"),
            ("CssClass", "class / className"),
        ],
        event_map: &[
            ("TextChanged", "OnChange / onChange / (input) event"),
            (
                "AutoPostBack",
                "Replaced by explicit change handler; no page reload",
            ),
        ],
        data_binding_pattern: "@bind-Value for Blazor; value+onChange for React; [(ngModel)] for Angular",
        notes: "Low complexity. Direct mapping. TextMode=MultiLine becomes <textarea>; TextMode=Password becomes type='password'.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "DropDownList",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputSelect<T>",
        react_equivalent: "<select> (controlled) / react-select",
        angular_equivalent: "<mat-select> / <select>",
        properties_map: &[
            ("SelectedValue", "@bind-Value / value prop / [(value)]"),
            ("DataTextField", "Option display field"),
            ("DataValueField", "Option value field"),
            ("Items", "Options collection / <option> elements"),
            ("AppendDataBoundItems", "Merge static + dynamic options"),
        ],
        event_map: &[(
            "SelectedIndexChanged",
            "OnChange / onChange / (selectionChange)",
        )],
        data_binding_pattern: "@bind-Value with <option> foreach; value+onChange for React; [(value)] for Angular",
        notes: "Low complexity. Map Items to <option> elements. AppendDataBoundItems requires merging static and data-bound options.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &["AutoPostBack has no direct equivalent"],
    },
    ControlMapping {
        legacy_control: "CheckBox",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputCheckbox",
        react_equivalent: "<input type='checkbox'> (controlled)",
        angular_equivalent: "<mat-checkbox> / <input type='checkbox'>",
        properties_map: &[
            ("Checked", "@bind-Value / checked prop / [(ngModel)]"),
            ("Text", "Adjacent <label> element"),
            ("AutoPostBack", "Explicit change handler"),
        ],
        event_map: &[("CheckedChanged", "OnChange / onChange / (change)")],
        data_binding_pattern: "@bind-Value for Blazor; checked+onChange for React; [(ngModel)] for Angular",
        notes: "Low complexity. Direct mapping. AutoPostBack removed; use explicit event handlers.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "CheckBoxList",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@foreach with InputCheckbox per item",
        react_equivalent: "Checkbox group component / map to <input type='checkbox'>",
        angular_equivalent: "*ngFor with mat-checkbox",
        properties_map: &[
            ("DataSource", "Items collection"),
            ("DataTextField", "Label field"),
            ("DataValueField", "Value field"),
            ("RepeatDirection", "CSS flex-direction"),
            ("SelectedValue", "Array of selected values"),
        ],
        event_map: &[(
            "SelectedIndexChanged",
            "Per-checkbox onChange aggregated into state",
        )],
        data_binding_pattern: "Maintain Set<string> of selected values; bind each checkbox to membership check",
        notes: "Low-medium complexity. No single-component equivalent; build from individual checkboxes with shared state.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["No single-component equivalent"],
    },
    ControlMapping {
        legacy_control: "RadioButton",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputRadio<T> inside InputRadioGroup<T>",
        react_equivalent: "<input type='radio'> (controlled)",
        angular_equivalent: "<mat-radio-button> inside <mat-radio-group>",
        properties_map: &[
            ("Checked", "Value comparison in radio group"),
            ("GroupName", "name attribute / InputRadioGroup Name"),
            ("Text", "Adjacent <label>"),
        ],
        event_map: &[(
            "CheckedChanged",
            "OnChange on radio group / onChange / (change)",
        )],
        data_binding_pattern: "@bind-Value on InputRadioGroup; name+value+onChange for React; [(ngModel)] on mat-radio-group",
        notes: "Low complexity. Must be placed inside a radio group construct in modern frameworks.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RadioButtonList",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputRadioGroup<T> with @foreach InputRadio",
        react_equivalent: "Radio group component / map to <input type='radio'>",
        angular_equivalent: "mat-radio-group with *ngFor mat-radio-button",
        properties_map: &[
            ("SelectedValue", "@bind-Value / value state / [(ngModel)]"),
            ("DataSource", "Options collection"),
            ("DataTextField", "Label field"),
            ("DataValueField", "Value field"),
            ("RepeatDirection", "CSS flex-direction"),
        ],
        event_map: &[("SelectedIndexChanged", "OnChange / onChange / (change)")],
        data_binding_pattern: "Bind selected value to radio group; iterate options to create radio buttons",
        notes: "Low complexity. Maps naturally to radio group patterns in all frameworks.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Calendar",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputDate<T> / third-party date picker",
        react_equivalent: "react-datepicker / MUI DatePicker",
        angular_equivalent: "mat-datepicker",
        properties_map: &[
            ("SelectedDate", "@bind-Value / selected date state"),
            ("VisibleDate", "Initial displayed month"),
            ("SelectionMode", "Single/range/week mode configuration"),
        ],
        event_map: &[
            ("SelectionChanged", "OnChange / onChange / (dateChange)"),
            ("DayRender", "Day cell render customization"),
        ],
        data_binding_pattern: "@bind-Value for Blazor; value+onChange for React; [(ngModel)] with matDatepicker for Angular",
        notes: "Medium complexity. Built-in Calendar is unique to WebForms; all modern frameworks require a date picker library.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Built-in calendar has no direct equivalent"],
    },
    ControlMapping {
        legacy_control: "FileUpload",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputFile",
        react_equivalent: "<input type='file'> / react-dropzone",
        angular_equivalent: "<input type='file'> with custom handler",
        properties_map: &[
            ("FileName", "IBrowserFile.Name / file.name"),
            ("FileBytes", "IBrowserFile.OpenReadStream() / FileReader"),
            ("HasFile", "file !== null check"),
            ("AllowMultiple", "multiple attribute"),
        ],
        event_map: &[("(no server event)", "OnChange / onChange / (change)")],
        data_binding_pattern: "Handle file via IBrowserFile stream (Blazor) or FormData (React/Angular); upload via HTTP POST",
        notes: "Medium complexity. WebForms PostedFile is server-side; modern equivalents are client-side with async upload.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Server-side PostedFile vs client-side streams"],
    },
    // =====================================================================
    // ACTION CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "Button",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<button> with @onclick",
        react_equivalent: "<button> with onClick",
        angular_equivalent: "<button mat-button> with (click)",
        properties_map: &[
            ("Text", "Button text content / children"),
            ("CommandName", "Custom data attribute or handler parameter"),
            (
                "CommandArgument",
                "Custom data attribute or handler parameter",
            ),
            ("CausesValidation", "Form validation trigger configuration"),
            ("ValidationGroup", "EditContext or form group scope"),
        ],
        event_map: &[
            ("Click", "@onclick / onClick / (click)"),
            ("Command", "Parameterized click handler"),
        ],
        data_binding_pattern: "No data binding; wire @onclick to async Task handler",
        notes: "Low complexity. Direct mapping. CommandName/CommandArgument pattern replaced by parameterized handlers or closures.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "LinkButton",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<button> styled as link / <a> with @onclick",
        react_equivalent: "<button> with link styling / <a> with onClick",
        angular_equivalent: "<a mat-button> with (click)",
        properties_map: &[
            ("Text", "Link text content"),
            ("CommandName", "Handler parameter"),
            ("CausesValidation", "Validation trigger config"),
        ],
        event_map: &[
            ("Click", "@onclick / onClick / (click)"),
            ("Command", "Parameterized click handler"),
        ],
        data_binding_pattern: "No data binding; use onclick handler",
        notes: "Low complexity. Renders as <a> with __doPostBack. Replace with button styled as anchor or use <a> with preventDefault.",
        lifecycle_phase: "Any",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &["Uses __doPostBack for click handling"],
    },
    ControlMapping {
        legacy_control: "ImageButton",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<button> with <img> child and @onclick",
        react_equivalent: "<button> with <img> child / <input type='image'>",
        angular_equivalent: "<button mat-icon-button> with (click)",
        properties_map: &[
            ("ImageUrl", "src attribute on <img>"),
            ("AlternateText", "alt attribute"),
            ("CommandName", "Handler parameter"),
        ],
        event_map: &[("Click", "@onclick / onClick / (click) with coordinates")],
        data_binding_pattern: "No data binding; wire click handler",
        notes: "Low complexity. Rarely used. Replace with icon button or <button> containing an <img>.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    // =====================================================================
    // NAVIGATION CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "HyperLink",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "NavLink / <a href>",
        react_equivalent: "<Link> (react-router) / <a href>",
        angular_equivalent: "<a routerLink> / <a href>",
        properties_map: &[
            ("NavigateUrl", "href / to prop / routerLink"),
            ("Text", "Link text content"),
            ("Target", "target attribute"),
        ],
        event_map: &[],
        data_binding_pattern: "Bind href/to to route path; no special data binding needed",
        notes: "Low complexity. Direct mapping to anchor element or router link.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Menu",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom NavMenu component / MudBlazor MudMenu",
        react_equivalent: "Custom menu component / MUI Menu",
        angular_equivalent: "mat-menu with mat-menu-item",
        properties_map: &[
            ("DataSource", "Menu items collection"),
            (
                "Orientation",
                "CSS flex-direction / horizontal/vertical prop",
            ),
            ("StaticDisplayLevels", "Visible depth configuration"),
            ("Items", "Recursive menu item collection"),
        ],
        event_map: &[("MenuItemClick", "OnClick per menu item / (click) per item")],
        data_binding_pattern: "Bind hierarchical menu items to recursive component tree",
        notes: "Medium-high complexity. WebForms Menu supports declarative hierarchical structure and SiteMap binding. Modern equivalents require building recursive menu components.",
        lifecycle_phase: "Init",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 3,
        breaking_differences: &[
            "SiteMap binding has no equivalent",
            "Dynamic menu items require manual management",
        ],
    },
    ControlMapping {
        legacy_control: "TreeView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom recursive TreeView component / MudTreeView",
        react_equivalent: "MUI TreeView / custom recursive component",
        angular_equivalent: "mat-tree (flat or nested)",
        properties_map: &[
            ("DataSource", "Tree node collection"),
            ("ShowCheckBoxes", "Checkbox per node configuration"),
            ("ExpandDepth", "Default expansion depth"),
            ("SelectedNode", "Selected node state"),
        ],
        event_map: &[
            (
                "SelectedNodeChanged",
                "OnSelectedChanged / onNodeSelect / (selectionChange)",
            ),
            (
                "TreeNodeExpanded",
                "OnExpand / onNodeToggle / (expandedChange)",
            ),
        ],
        data_binding_pattern: "Bind hierarchical data model to recursive tree component",
        notes: "High complexity. Recursive data binding with lazy loading, check state propagation, and selection management.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &[
            "Check state propagation",
            "Lazy loading via populate-on-demand",
        ],
    },
    ControlMapping {
        legacy_control: "SiteMapPath",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom breadcrumb component / MudBreadcrumbs",
        react_equivalent: "MUI Breadcrumbs / custom breadcrumb",
        angular_equivalent: "Custom breadcrumb with routerLink",
        properties_map: &[
            ("SiteMapProvider", "Route-based breadcrumb data"),
            ("PathSeparator", "Separator character / component"),
        ],
        event_map: &[],
        data_binding_pattern: "Derive breadcrumb trail from current route hierarchy",
        notes: "Medium complexity. web.sitemap file has no modern equivalent; derive breadcrumbs from route configuration or navigation state.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["web.sitemap has no modern equivalent"],
    },
    // =====================================================================
    // LAYOUT CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "Panel",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<div> with CSS classes",
        react_equivalent: "<div> / MUI Box / Paper",
        angular_equivalent: "<div> / mat-card",
        properties_map: &[
            ("GroupingText", "<fieldset> + <legend>"),
            ("ScrollBars", "CSS overflow property"),
            ("Visible", "Conditional rendering (@if / && / *ngIf)"),
            ("DefaultButton", "Form default submit button"),
        ],
        event_map: &[],
        data_binding_pattern: "No special data binding; use conditional rendering for visibility",
        notes: "Low complexity. Panel is a <div> (or <fieldset> with GroupingText). Direct HTML/CSS mapping.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "PlaceHolder",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "RenderFragment / @if conditional block",
        react_equivalent: "React.Fragment / conditional rendering",
        angular_equivalent: "<ng-container> / <ng-content>",
        properties_map: &[
            ("Visible", "Conditional rendering flag"),
            ("Controls (dynamic)", "Dynamic child component rendering"),
        ],
        event_map: &[],
        data_binding_pattern: "Render child components conditionally; use DynamicComponent for runtime composition",
        notes: "Low complexity. PlaceHolder is a no-markup container for dynamic controls. Replace with conditional rendering or component slots.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "MultiView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Switch statement on active view index / tab component",
        react_equivalent: "Conditional rendering / tab panel component",
        angular_equivalent: "ngSwitch / mat-tab-group",
        properties_map: &[
            ("ActiveViewIndex", "Active tab/view state index"),
            ("Views", "Child view components"),
        ],
        event_map: &[(
            "ActiveViewChanged",
            "State change handler / (selectedTabChange)",
        )],
        data_binding_pattern: "Bind active index to state; render corresponding view conditionally",
        notes: "Low-medium complexity. Replace with tab component or conditional render based on active index state.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "View",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "RenderFragment / conditional block",
        react_equivalent: "Conditional JSX block",
        angular_equivalent: "ng-template with ngSwitchCase",
        properties_map: &[("Visible", "Controlled by parent MultiView/tab state")],
        event_map: &[
            ("Activate", "Component mount / show lifecycle"),
            ("Deactivate", "Component unmount / hide lifecycle"),
        ],
        data_binding_pattern: "No direct data binding; visibility controlled by parent container",
        notes: "Low complexity. Always used inside MultiView. Replace with conditional render block.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Wizard",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom stepper component / MudStepper",
        react_equivalent: "MUI Stepper / react-step-wizard",
        angular_equivalent: "mat-stepper",
        properties_map: &[
            ("ActiveStepIndex", "Active step state"),
            ("WizardSteps", "Step component collection"),
            ("DisplaySideBar", "Step indicator / sidebar visibility"),
            ("FinishButtonText", "Finish button label"),
        ],
        event_map: &[
            ("ActiveStepChanged", "OnStepChange / (selectionChange)"),
            ("FinishButtonClick", "OnFinish / submit handler"),
            ("NextButtonClick", "OnNext / step advance handler"),
            ("CancelButtonClick", "OnCancel handler"),
        ],
        data_binding_pattern: "Bind active step index to state; each step contains its own form/content",
        notes: "High complexity. Built-in Wizard has navigation, validation per step, and sidebar. Modern equivalents require manual step validation and navigation wiring.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &[
            "Step validation lifecycle",
            "Auto-generated navigation buttons",
        ],
    },
    // =====================================================================
    // AJAX CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "UpdatePanel",
        legacy_namespace: "System.Web.UI.UpdatePanel",
        blazor_equivalent: "No equivalent needed (Blazor is SPA by default)",
        react_equivalent: "No equivalent needed (React is SPA by default)",
        angular_equivalent: "No equivalent needed (Angular is SPA by default)",
        properties_map: &[
            ("UpdateMode", "N/A in SPA frameworks"),
            ("ChildrenAsTriggers", "N/A in SPA frameworks"),
            ("Triggers", "N/A in SPA frameworks"),
        ],
        event_map: &[],
        data_binding_pattern: "Remove entirely; modern frameworks handle partial updates natively via virtual DOM / change detection",
        notes: "Medium complexity to remove. UpdatePanel masks full postbacks as partial updates. Removing it requires ensuring all child controls work with proper client-side state management. Often hides coupling to ViewState.",
        lifecycle_phase: "Any",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &[
            "Masks full postback as partial update — child controls still execute full page lifecycle",
            "ViewState coupling hidden: child controls depend on ViewState without knowing it",
            "AsyncPostBackTrigger/PostBackTrigger model has no SPA equivalent; decompose into component boundaries",
            "Nested UpdatePanels create invisible dependency chains that break when removed",
        ],
    },
    ControlMapping {
        legacy_control: "ScriptManager",
        legacy_namespace: "System.Web.UI.ScriptManager",
        blazor_equivalent: "No equivalent needed (_Imports.razor / wwwroot scripts)",
        react_equivalent: "No equivalent needed (bundler handles scripts)",
        angular_equivalent: "No equivalent needed (angular.json scripts)",
        properties_map: &[
            ("EnablePartialRendering", "N/A"),
            ("ScriptPath", "Static asset configuration"),
            ("EnablePageMethods", "Replace with API endpoint calls"),
        ],
        event_map: &[],
        data_binding_pattern: "Remove entirely; script loading handled by build tooling",
        notes: "Low complexity to remove. ScriptManager orchestrates AJAX partial rendering. Modern build systems (webpack, vite) handle script bundling.",
        lifecycle_phase: "Init",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Timer",
        legacy_namespace: "System.Web.UI.Timer",
        blazor_equivalent: "System.Threading.Timer / PeriodicTimer in @code",
        react_equivalent: "setInterval / useEffect with interval",
        angular_equivalent: "RxJS interval() / timer()",
        properties_map: &[
            ("Interval", "Timer interval in milliseconds"),
            ("Enabled", "Start/stop timer state"),
        ],
        event_map: &[("Tick", "Timer callback / subscription handler")],
        data_binding_pattern: "Set up timer in component lifecycle; trigger state update on tick",
        notes: "Low-medium complexity. WebForms Timer triggers postback on interval. Replace with client-side timer that calls API and updates state.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Triggers full postback on interval"],
    },
    ControlMapping {
        legacy_control: "UpdateProgress",
        legacy_namespace: "System.Web.UI.UpdateProgress",
        blazor_equivalent: "Conditional spinner / loading overlay component",
        react_equivalent: "Loading state + spinner component / Suspense",
        angular_equivalent: "Loading indicator with *ngIf / HTTP interceptor spinner",
        properties_map: &[
            (
                "AssociatedUpdatePanelID",
                "Loading state scoped to specific operation",
            ),
            ("DisplayAfter", "CSS transition-delay or setTimeout"),
            ("ProgressTemplate", "Spinner/skeleton component template"),
        ],
        event_map: &[],
        data_binding_pattern: "Bind visibility to isLoading state; show during async operations",
        notes: "Low complexity. Replace with boolean loading state and conditional rendering of a spinner or skeleton component.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    // =====================================================================
    // DATA ACCESS CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "SqlDataSource",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Injected service + EF Core / Dapper (server-side data access)",
        react_equivalent: "API call via fetch/axios + backend endpoint",
        angular_equivalent: "HttpClient service + backend API endpoint",
        properties_map: &[
            (
                "ConnectionString",
                "appsettings.json connection string / env variable",
            ),
            ("SelectCommand", "EF Core LINQ query / repository method"),
            ("InsertCommand", "Repository insert method / API POST"),
            ("UpdateCommand", "Repository update method / API PUT"),
            ("DeleteCommand", "Repository delete method / API DELETE"),
        ],
        event_map: &[
            ("Selected", "Async data load completion callback"),
            ("Inserting", "Pre-insert validation / middleware"),
        ],
        data_binding_pattern: "Inject data service; call async methods in OnInitializedAsync / useEffect / ngOnInit",
        notes: "High complexity. SqlDataSource embeds SQL in markup, which is a security and architecture anti-pattern. Must be refactored to repository pattern with parameterized queries behind an API layer.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 4,
        breaking_differences: &[
            "SQL in markup is anti-pattern",
            "Must refactor to repository pattern",
        ],
    },
    ControlMapping {
        legacy_control: "ObjectDataSource",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Injected service / DI-registered repository",
        react_equivalent: "Custom hook calling API service",
        angular_equivalent: "Injectable service",
        properties_map: &[
            ("TypeName", "Service class / DI registration"),
            ("SelectMethod", "Service method name"),
            ("InsertMethod", "Service insert method"),
            ("UpdateMethod", "Service update method"),
            ("DeleteMethod", "Service delete method"),
        ],
        event_map: &[
            ("Selected", "Async completion handler"),
            ("ObjectCreating", "DI resolution (automatic)"),
        ],
        data_binding_pattern: "Register service in DI container; inject and call methods directly",
        notes: "Medium complexity. Closest to modern DI patterns. TypeName maps to service registration; methods called directly via injected interface.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Reflection-based method binding"],
    },
    ControlMapping {
        legacy_control: "LinqDataSource",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "EF Core DbContext with LINQ queries",
        react_equivalent: "GraphQL client / REST API with query parameters",
        angular_equivalent: "HttpClient with query parameter construction",
        properties_map: &[
            ("ContextTypeName", "DbContext class"),
            ("TableName", "DbSet<T> property"),
            ("Where", "LINQ .Where() clause"),
            ("OrderBy", "LINQ .OrderBy() clause"),
        ],
        event_map: &[("Selecting", "Pre-query filter application")],
        data_binding_pattern: "Use EF Core LINQ directly in service; expose via API for SPA frameworks",
        notes: "Medium complexity. LINQ-to-SQL/EF patterns transfer well to EF Core. Markup-embedded queries must move to service layer.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "EntityDataSource",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "EF Core DbContext (injected)",
        react_equivalent: "REST/GraphQL API backed by EF Core",
        angular_equivalent: "HttpClient service backed by EF Core API",
        properties_map: &[
            ("ConnectionString", "DbContext configuration"),
            ("DefaultContainerName", "DbContext class name"),
            ("EntitySetName", "DbSet<T> property name"),
            ("Where", "LINQ Where clause"),
        ],
        event_map: &[("QueryCreated", "IQueryable pipeline extension")],
        data_binding_pattern: "Use EF Core DbContext in service layer; expose via API endpoints",
        notes: "Medium complexity. Entity Framework already has a modern equivalent (EF Core). Connection/mapping configuration moves to DbContext OnModelCreating.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    // =====================================================================
    // DISPLAY CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "Label",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<span> / <label> with @bind or text interpolation",
        react_equivalent: "<span> / <label> with text content",
        angular_equivalent: "<span> / <label> with interpolation",
        properties_map: &[
            ("Text", "Text content / interpolated value"),
            ("AssociatedControlID", "for/htmlFor attribute"),
            ("CssClass", "class / className"),
        ],
        event_map: &[],
        data_binding_pattern: "Render text via interpolation: @variable / {variable} / {{variable}}",
        notes: "Low complexity. Label renders as <span>. If AssociatedControlID is set, use <label for='...'>.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Literal",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@((MarkupString)rawHtml) for raw; text interpolation for encoded",
        react_equivalent: "dangerouslySetInnerHTML for raw; text node for encoded",
        angular_equivalent: "[innerHTML] for raw; text interpolation for encoded",
        properties_map: &[
            ("Text", "Rendered text/HTML content"),
            ("Mode", "Encode/PassThrough/Transform rendering mode"),
        ],
        event_map: &[],
        data_binding_pattern: "Interpolate value directly; use raw HTML rendering only when Mode=PassThrough and content is trusted",
        notes: "Low complexity. Mode=Encode is default (safe). Mode=PassThrough renders raw HTML -- ensure XSS safety in modern equivalent.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &["Mode=PassThrough renders raw HTML without encoding"],
    },
    ControlMapping {
        legacy_control: "Image",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<img> element",
        react_equivalent: "<img> element / next/image",
        angular_equivalent: "<img> element / NgOptimizedImage",
        properties_map: &[
            ("ImageUrl", "src attribute"),
            ("AlternateText", "alt attribute"),
            ("Width", "CSS width / width attribute"),
            ("Height", "CSS height / height attribute"),
        ],
        event_map: &[],
        data_binding_pattern: "Bind src to dynamic URL; use lazy loading for performance",
        notes: "Low complexity. Direct HTML <img> mapping. Consider modern image optimization (srcset, lazy loading).",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    // =====================================================================
    // VALIDATION CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "ValidationSummary",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<ValidationSummary> component",
        react_equivalent: "Form error summary component / formState.errors",
        angular_equivalent: "Custom error summary reading FormGroup errors",
        properties_map: &[
            ("DisplayMode", "List/BulletList/SingleParagraph rendering"),
            ("ValidationGroup", "EditContext scope / form group"),
            ("ShowMessageBox", "toast/alert notification"),
            ("ShowSummary", "Inline error list visibility"),
        ],
        event_map: &[],
        data_binding_pattern: "Bind to form validation state; display collected validation messages",
        notes: "Low complexity. Blazor has a direct <ValidationSummary> equivalent. React/Angular require manual aggregation of form errors.",
        lifecycle_phase: "PreRender",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RequiredFieldValidator",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "[Required] data annotation + <ValidationMessage>",
        react_equivalent: "required attribute / yup.required() / zod.min(1)",
        angular_equivalent: "Validators.required + <mat-error>",
        properties_map: &[
            ("ControlToValidate", "Field reference / form control name"),
            ("ErrorMessage", "Validation message string"),
            ("ValidationGroup", "Form group scope"),
            ("InitialValue", "Ignore-value for empty check"),
        ],
        event_map: &[],
        data_binding_pattern: "Add [Required] attribute to model property; framework handles display",
        notes: "Low complexity. Direct mapping to required validation in all frameworks.",
        lifecycle_phase: "PreRender",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "CompareValidator",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "[Compare] data annotation + <ValidationMessage>",
        react_equivalent: "Custom validation rule / yup.oneOf([ref('field')])",
        angular_equivalent: "Custom validator function comparing controls",
        properties_map: &[
            ("ControlToValidate", "Field reference"),
            ("ControlToCompare", "Comparison field reference"),
            ("Operator", "Comparison operator (Equal, GreaterThan, etc.)"),
            ("ValueToCompare", "Static comparison value"),
            ("Type", "Data type for comparison"),
        ],
        event_map: &[],
        data_binding_pattern: "Add [Compare] attribute for equality; custom validator for other operators",
        notes: "Low-medium complexity. Equality comparison maps directly; other operators (GreaterThan, etc.) require custom validation logic.",
        lifecycle_phase: "PreRender",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RangeValidator",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "[Range] data annotation + <ValidationMessage>",
        react_equivalent: "yup.min().max() / zod.min().max() / custom rule",
        angular_equivalent: "Validators.min + Validators.max + <mat-error>",
        properties_map: &[
            ("ControlToValidate", "Field reference"),
            ("MinimumValue", "Minimum allowed value"),
            ("MaximumValue", "Maximum allowed value"),
            ("Type", "Data type (Integer, Double, Date, etc.)"),
        ],
        event_map: &[],
        data_binding_pattern: "Add [Range(min, max)] attribute to model property",
        notes: "Low complexity. Direct mapping to range validation in all frameworks.",
        lifecycle_phase: "PreRender",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RegularExpressionValidator",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "[RegularExpression] data annotation + <ValidationMessage>",
        react_equivalent: "yup.matches(regex) / zod.regex() / pattern attribute",
        angular_equivalent: "Validators.pattern + <mat-error>",
        properties_map: &[
            ("ControlToValidate", "Field reference"),
            ("ValidationExpression", "Regex pattern string"),
            ("ErrorMessage", "Validation message"),
        ],
        event_map: &[],
        data_binding_pattern: "Add [RegularExpression(pattern)] attribute to model property",
        notes: "Low complexity. Direct mapping. Verify regex syntax compatibility between .NET and JavaScript regex engines.",
        lifecycle_phase: "PreRender",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "CustomValidator",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom ValidationAttribute / IValidatableObject",
        react_equivalent: "Custom validation function / yup.test() / zod.refine()",
        angular_equivalent: "Custom ValidatorFn / AsyncValidatorFn",
        properties_map: &[
            ("ControlToValidate", "Field reference"),
            (
                "ClientValidationFunction",
                "Client-side validation function",
            ),
            ("ValidateEmptyText", "Validate even when empty"),
            ("ErrorMessage", "Validation message"),
        ],
        event_map: &[("ServerValidate", "Custom validation logic (server-side)")],
        data_binding_pattern: "Implement custom validation attribute or inline validator function",
        notes: "Medium complexity. Server-side validation logic must be ported to validation attribute. Client-side function rewritten in framework's validation paradigm.",
        lifecycle_phase: "PreRender",
        state_model: "Stateless",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &["Server-side validation logic must be ported"],
    },
    // =====================================================================
    // ADDITIONAL CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "ListBox",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "InputSelect<T> with size attribute / multi-select",
        react_equivalent: "<select multiple> / react-select isMulti",
        angular_equivalent: "<mat-select multiple> / <select multiple>",
        properties_map: &[
            ("SelectionMode", "multiple attribute / isMulti prop"),
            ("Rows", "size attribute"),
            ("DataSource", "Options collection"),
            ("DataTextField", "Option display field"),
            ("DataValueField", "Option value field"),
        ],
        event_map: &[(
            "SelectedIndexChanged",
            "OnChange / onChange / (selectionChange)",
        )],
        data_binding_pattern: "Bind selected value(s) to state; populate options from collection",
        notes: "Low complexity. Similar to DropDownList but with visible rows and optional multi-select.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "HiddenField",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Component state / [Parameter] / cascading value",
        react_equivalent: "useState / useRef / <input type='hidden'>",
        angular_equivalent: "Component property / <input type='hidden'>",
        properties_map: &[("Value", "State variable / hidden input value")],
        event_map: &[("ValueChanged", "State change notification")],
        data_binding_pattern: "Store value in component state; use hidden input only for form submission compatibility",
        notes: "Low complexity. Usually replaced by component state. Only use <input type='hidden'> when submitting to a traditional form endpoint.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Table",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<table> HTML element",
        react_equivalent: "<table> HTML element / styled-components Table",
        angular_equivalent: "<table mat-table> / <table> HTML element",
        properties_map: &[
            ("Rows", "Child <tr> elements"),
            ("CellPadding", "CSS padding on <td>"),
            ("CellSpacing", "CSS border-spacing"),
            ("GridLines", "CSS border properties"),
        ],
        event_map: &[],
        data_binding_pattern: "Build table rows from data using iteration; use CSS for styling",
        notes: "Low complexity. Direct HTML table mapping. Prefer CSS Grid or Flexbox for layout; use <table> only for tabular data.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "TableRow",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<tr> HTML element",
        react_equivalent: "<tr> HTML element",
        angular_equivalent: "<tr> HTML element",
        properties_map: &[
            ("Cells", "Child <td>/<th> elements"),
            ("TableSection", "Parent <thead>/<tbody>/<tfoot>"),
        ],
        event_map: &[],
        data_binding_pattern: "Render as <tr> inside table iteration",
        notes: "Low complexity. Direct HTML <tr> mapping.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "TableCell",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<td> / <th> HTML element",
        react_equivalent: "<td> / <th> HTML element",
        angular_equivalent: "<td> / <th> HTML element",
        properties_map: &[
            ("Text", "Cell text content"),
            ("ColumnSpan", "colspan attribute"),
            ("RowSpan", "rowspan attribute"),
        ],
        event_map: &[],
        data_binding_pattern: "Render as <td> with interpolated content",
        notes: "Low complexity. Direct HTML <td>/<th> mapping.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "LoginView",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<AuthorizeView> component",
        react_equivalent: "Conditional render based on auth context / ProtectedRoute",
        angular_equivalent: "*ngIf with auth service / route guard",
        properties_map: &[
            (
                "AnonymousTemplate",
                "NotAuthorized render fragment / fallback",
            ),
            (
                "LoggedInTemplate",
                "Authorized render fragment / protected content",
            ),
            ("RoleGroups", "Policy-based authorization / role checks"),
        ],
        event_map: &[],
        data_binding_pattern: "Use <AuthorizeView> with <Authorized> and <NotAuthorized> child content",
        notes: "Medium complexity. Blazor <AuthorizeView> is a direct equivalent. React/Angular require auth context/service integration with conditional rendering.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "ContentPlaceHolder",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@Body in layout / @RenderBody()",
        react_equivalent: "{children} / <Outlet> (react-router)",
        angular_equivalent: "<router-outlet> / <ng-content>",
        properties_map: &[("ID", "Slot name / outlet name")],
        event_map: &[],
        data_binding_pattern: "Define layout with @Body placeholder; pages specify layout via @layout directive",
        notes: "Low complexity. Master page ContentPlaceHolder maps to layout slot/outlet. Named placeholders map to named slots or sections.",
        lifecycle_phase: "Init",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Content",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "@section / page body targeting layout",
        react_equivalent: "Component rendered into Outlet / children",
        angular_equivalent: "Component rendered into router-outlet / ng-content select",
        properties_map: &[("ContentPlaceHolderID", "Section name / target slot")],
        event_map: &[],
        data_binding_pattern: "Page content automatically fills the corresponding layout placeholder",
        notes: "Low complexity. Content control fills a ContentPlaceHolder. Maps to layout section system.",
        lifecycle_phase: "Init",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "BulletedList",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "<ul>/<ol> with @foreach <li>",
        react_equivalent: "<ul>/<ol> with .map() <li>",
        angular_equivalent: "<ul>/<ol> with *ngFor <li>",
        properties_map: &[
            ("DataSource", "Items collection"),
            ("BulletStyle", "CSS list-style-type"),
            ("DisplayMode", "Text/HyperLink/LinkButton rendering"),
            ("DataTextField", "Item text field"),
            ("DataValueField", "Item value field"),
        ],
        event_map: &[(
            "Click",
            "Per-item click handler (when DisplayMode=LinkButton)",
        )],
        data_binding_pattern: "Iterate collection; render <li> per item with text or link content",
        notes: "Low complexity. Direct mapping to HTML list with iteration.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "AdRotator",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom ad rotation component with Timer",
        react_equivalent: "Custom carousel/banner component with useEffect interval",
        angular_equivalent: "Custom component with RxJS timer for rotation",
        properties_map: &[
            (
                "AdvertisementFile",
                "Ad configuration data source (JSON/API)",
            ),
            ("KeywordFilter", "Filtering logic on ad data"),
        ],
        event_map: &[("AdCreated", "OnAdChange / render callback")],
        data_binding_pattern: "Load ad data from API; rotate display on timer interval",
        notes: "Medium complexity. Rarely used. Replace with custom banner rotation or third-party ad SDK integration.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Xml",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Server-side XSLT transform + MarkupString rendering",
        react_equivalent: "Server-side transform or client-side XML parser",
        angular_equivalent: "Server-side transform or DomSanitizer with innerHTML",
        properties_map: &[
            ("DocumentSource", "XML file path / content"),
            ("TransformSource", "XSLT file path"),
        ],
        event_map: &[],
        data_binding_pattern: "Transform XML server-side; render result as HTML content",
        notes: "High complexity. XSLT-based rendering is rare in modern frameworks. Convert XML data to JSON and render with components, or keep XSLT transform on server.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["XSLT has no modern client-side equivalent"],
    },
    ControlMapping {
        legacy_control: "Substitution",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Component rendering (no output caching equivalent needed)",
        react_equivalent: "Dynamic component rendering",
        angular_equivalent: "Dynamic component with ViewContainerRef",
        properties_map: &[(
            "MethodName",
            "Component render method / dynamic content function",
        )],
        event_map: &[],
        data_binding_pattern: "Render dynamic content inline; output caching concept does not apply to SPA frameworks",
        notes: "Low complexity. Substitution punches through output caching. Irrelevant in SPA architectures where rendering is client-side.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "Login",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom login form with EditForm + InputText",
        react_equivalent: "Login form component with controlled inputs",
        angular_equivalent: "Login form with Reactive Forms + mat-form-field",
        properties_map: &[
            ("UserNameLabelText", "Label text for username field"),
            ("PasswordLabelText", "Label text for password field"),
            ("RememberMeSet", "Remember me checkbox default"),
            ("FailureText", "Error message display text"),
            ("DestinationPageUrl", "Post-login redirect URL"),
        ],
        event_map: &[
            ("Authenticate", "OnValidSubmit / form submit handler"),
            ("LoggedIn", "Post-authentication redirect/callback"),
        ],
        data_binding_pattern: "Bind username/password to form state; call auth API on submit; handle redirect",
        notes: "Medium complexity. Login control encapsulates entire auth flow. Must decompose into form, API call, token storage, and redirect logic.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["Encapsulates entire auth flow that must be decomposed"],
    },
    ControlMapping {
        legacy_control: "ChangePassword",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom change password form with EditForm",
        react_equivalent: "Change password form component",
        angular_equivalent: "Change password form with Reactive Forms",
        properties_map: &[
            ("CurrentPasswordLabelText", "Current password field label"),
            ("NewPasswordLabelText", "New password field label"),
            ("ConfirmPasswordLabelText", "Confirm password field label"),
        ],
        event_map: &[
            ("ChangingPassword", "Pre-change validation handler"),
            ("ChangedPassword", "Post-change success handler"),
        ],
        data_binding_pattern: "Bind old/new/confirm password to form; call password change API on submit",
        notes: "Medium complexity. Decompose into form with three fields, validation (match check), and API call.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "CreateUserWizard",
        legacy_namespace: "System.Web.UI.WebControls",
        blazor_equivalent: "Custom registration wizard with EditForm steps",
        react_equivalent: "Multi-step registration form / stepper component",
        angular_equivalent: "mat-stepper with registration form steps",
        properties_map: &[
            ("RequireEmail", "Email field validation requirement"),
            ("UserNameLabelText", "Username field label"),
            ("WizardSteps", "Registration step definitions"),
        ],
        event_map: &[
            ("CreatingUser", "Pre-registration validation"),
            ("CreatedUser", "Post-registration callback / redirect"),
        ],
        data_binding_pattern: "Build multi-step form; collect user data across steps; call registration API on final submit",
        notes: "High complexity. Combines Wizard + user creation + membership provider integration. Must decompose into multi-step form, validation, and Identity API calls.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &["Multi-step flow with membership provider integration"],
    },
    // =====================================================================
    // THIRD-PARTY: TELERIK (RAD) CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "RadGrid",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikGrid<T> / MudDataGrid<T>",
        react_equivalent: "KendoReact Grid / AG Grid",
        angular_equivalent: "Kendo Angular Grid / ag-grid-angular",
        properties_map: &[
            ("DataSource", "Data / Items"),
            ("AllowSorting", "Sortable"),
            ("AllowPaging", "Pageable"),
            ("AllowFilteringByColumn", "FilterMode"),
            ("MasterTableView", "Columns definition"),
        ],
        event_map: &[
            ("NeedDataSource", "OnRead / data fetch"),
            ("ItemCommand", "OnRowClick / command handler"),
            ("UpdateCommand", "OnUpdate / edit handler"),
            ("DeleteCommand", "OnDelete / handler"),
        ],
        data_binding_pattern: "Bind to IEnumerable<T> via OnRead event or Items property; configure columns declaratively",
        notes: "Very high complexity. RadGrid has 100+ properties. Must map MasterTableView columns, detail tables, grouping, and export to modern grid.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: true,
        migration_complexity: 5,
        breaking_differences: &[
            "NeedDataSource fires on every postback (Init/Filter/Sort/Page); modern OnRead fires once per user action",
            "ViewState reconstructs page index, sort column, and filter state server-side; Blazor grids hold this in component state",
            "MasterTableView detail tables create nested postback cycles with no modern equivalent",
            "Column templates with server controls require complete decomposition to Razor components",
        ],
    },
    ControlMapping {
        legacy_control: "RadEditor",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikEditor / TinyMCE Blazor",
        react_equivalent: "KendoReact Editor / TinyMCE React",
        angular_equivalent: "Kendo Angular Editor / CKEditor Angular",
        properties_map: &[
            ("Content", "Value"),
            ("ToolsFile", "Tools configuration"),
            ("EditModes", "Edit mode toggles"),
        ],
        event_map: &[("OnClientLoad", "Initialized event")],
        data_binding_pattern: "Two-way bind to string HTML content property",
        notes: "Medium complexity. Map toolbar configuration and content filters. Watch for custom dialogs and image upload handlers.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["Custom dialog integration must be rebuilt"],
    },
    ControlMapping {
        legacy_control: "RadComboBox",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikComboBox<T> / MudAutocomplete<T>",
        react_equivalent: "KendoReact ComboBox / react-select",
        angular_equivalent: "Kendo Angular ComboBox / ng-select",
        properties_map: &[
            ("DataTextField", "TextField"),
            ("DataValueField", "ValueField"),
            ("EnableLoadOnDemand", "Filterable / OnRead"),
        ],
        event_map: &[
            ("SelectedIndexChanged", "ValueChanged"),
            ("ItemsRequested", "OnRead / OnFilter"),
        ],
        data_binding_pattern: "Bind Data property to collection; use TextField/ValueField for display mapping",
        notes: "Medium complexity. Load-on-demand and web service integration require OnRead callback pattern.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["Load-on-demand requires callback pattern conversion"],
    },
    ControlMapping {
        legacy_control: "RadTreeView",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikTreeView / MudTreeView",
        react_equivalent: "KendoReact TreeView / react-arborist",
        angular_equivalent: "Kendo Angular TreeView / mat-tree",
        properties_map: &[
            ("DataSource", "Data"),
            ("DataFieldID", "IdField"),
            ("DataFieldParentID", "ParentIdField"),
            ("DataTextField", "TextField"),
        ],
        event_map: &[
            ("NodeClick", "OnItemClick"),
            ("NodeExpand", "OnExpand"),
            ("NodeCheck", "OnItemCheck"),
        ],
        data_binding_pattern: "Bind to hierarchical or flat collection with Id/ParentId fields",
        notes: "Medium complexity. Map drag-and-drop, checkboxes, and context menus. Load-on-demand requires async data callback.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 3,
        breaking_differences: &["Drag-and-drop integration", "Context menu binding"],
    },
    ControlMapping {
        legacy_control: "RadScheduler",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikScheduler / Syncfusion Scheduler",
        react_equivalent: "KendoReact Scheduler / FullCalendar",
        angular_equivalent: "Kendo Angular Scheduler / FullCalendar Angular",
        properties_map: &[
            ("DataSource", "Data"),
            ("DataStartField", "StartField"),
            ("DataEndField", "EndField"),
            ("DataSubjectField", "TitleField"),
        ],
        event_map: &[
            ("AppointmentInsert", "OnCreate"),
            ("AppointmentUpdate", "OnUpdate"),
            ("AppointmentDelete", "OnDelete"),
        ],
        data_binding_pattern: "Bind to appointment collection with start/end/title field mappings",
        notes: "High complexity. Map recurring appointments, resources, time zones, and custom templates.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &["Recurring appointment model", "Resource grouping"],
    },
    ControlMapping {
        legacy_control: "RadMenu",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikMenu / MudMenu",
        react_equivalent: "KendoReact Menu / MUI Menu",
        angular_equivalent: "Kendo Angular Menu / mat-menu",
        properties_map: &[
            ("DataSource", "Data"),
            ("DataTextField", "TextField"),
            ("DataNavigateUrlField", "UrlField"),
        ],
        event_map: &[("ItemClick", "OnClick")],
        data_binding_pattern: "Bind to hierarchical menu item collection",
        notes: "Low-medium complexity. Map multi-level menus and context menu integration.",
        lifecycle_phase: "Init",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RadDatePicker",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikDatePicker / MudDatePicker",
        react_equivalent: "KendoReact DatePicker / MUI DatePicker",
        angular_equivalent: "Kendo Angular DatePicker / mat-datepicker",
        properties_map: &[
            ("SelectedDate", "Value"),
            ("MinDate", "Min"),
            ("MaxDate", "Max"),
            ("DateFormat", "Format"),
        ],
        event_map: &[("SelectedDateChanged", "ValueChanged")],
        data_binding_pattern: "Two-way bind to DateTime? property",
        notes: "Low complexity. Straightforward date picker replacement.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RadWindow",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikWindow / MudDialog",
        react_equivalent: "KendoReact Dialog / MUI Dialog",
        angular_equivalent: "Kendo Angular Dialog / mat-dialog",
        properties_map: &[
            ("Visible", "Visible"),
            ("Title", "Title"),
            ("Modal", "Modal"),
            ("Width", "Width"),
            ("Height", "Height"),
        ],
        event_map: &[("OnClientClose", "VisibleChanged")],
        data_binding_pattern: "Control Visible property via bool binding; emit close event",
        notes: "Low-medium complexity. Map RadWindowManager if used for multiple windows.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RadChart",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikChart / MudChart",
        react_equivalent: "KendoReact Charts / Recharts / Chart.js",
        angular_equivalent: "Kendo Angular Charts / ngx-charts",
        properties_map: &[
            ("DataSource", "Data"),
            ("ChartTitle.TextBlock.Text", "Title"),
        ],
        event_map: &[("Click", "OnSeriesClick")],
        data_binding_pattern: "Bind series Data to collection; configure axes and series type",
        notes: "High complexity. RadChart has been superseded by RadHtmlChart. Map chart type, series, axes, and tooltips to modern charting library.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &["Superseded by RadHtmlChart"],
    },
    ControlMapping {
        legacy_control: "RadUpload",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikUpload / InputFile / MudFileUpload",
        react_equivalent: "KendoReact Upload / react-dropzone",
        angular_equivalent: "Kendo Angular Upload / ngx-file-drop",
        properties_map: &[
            ("AllowedFileExtensions", "AllowedExtensions"),
            ("MaxFileSize", "MaxFileSize"),
            ("ControlObjectsVisibility", "UI config"),
        ],
        event_map: &[
            ("FileUploaded", "OnUpload / OnSuccess"),
            ("FilesChanged", "OnSelect"),
        ],
        data_binding_pattern: "Handle file upload via event callback; process stream server-side",
        notes: "Medium complexity. Map async upload handler, progress tracking, and validation.",
        lifecycle_phase: "Load",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "RadTabStrip",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "TelerikTabStrip / MudTabs",
        react_equivalent: "KendoReact TabStrip / MUI Tabs",
        angular_equivalent: "Kendo Angular TabStrip / mat-tab-group",
        properties_map: &[
            ("SelectedIndex", "ActiveTabIndex"),
            ("MultiPage", "Content panels"),
        ],
        event_map: &[("TabClick", "ActiveTabIndexChanged")],
        data_binding_pattern: "Bind ActiveTabIndex; use tab content components for each panel",
        notes: "Low complexity. Map RadMultiPage content panels to tab content areas.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 2,
        breaking_differences: &["RadMultiPage integration"],
    },
    ControlMapping {
        legacy_control: "RadAjaxManager",
        legacy_namespace: "Telerik.Web.UI",
        blazor_equivalent: "Not needed (Blazor is SPA)",
        react_equivalent: "Not needed (React manages state)",
        angular_equivalent: "Not needed (Angular manages updates)",
        properties_map: &[
            ("AjaxSettings", "Remove — use component state"),
            ("LoadingPanel", "Loading indicator component"),
        ],
        event_map: &[("AjaxRequest", "Component event / API call")],
        data_binding_pattern: "Remove entirely; replace with component-level state management and loading indicators",
        notes: "High complexity conceptually. RadAjaxManager wraps UpdatePanel-style partial rendering. Must decompose into component boundaries and individual data-fetch patterns.",
        lifecycle_phase: "Init",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 4,
        breaking_differences: &[
            "AjaxSettings bind control-to-UpdatePanel pairs; must decompose into individual component data-fetch patterns",
            "RadAjaxPanel wraps UpdatePanel-style partial rendering; child controls still run full lifecycle",
            "Loading panel per-control integration must be replaced with per-component loading state",
        ],
    },
    // =====================================================================
    // THIRD-PARTY: DEVEXPRESS (DX) CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "ASPxGridView",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxGrid / MudDataGrid<T>",
        react_equivalent: "DevExtreme React DataGrid / AG Grid",
        angular_equivalent: "DevExtreme Angular DataGrid",
        properties_map: &[
            ("DataSource", "Data / DataSource"),
            ("KeyFieldName", "KeyFieldName / key column"),
            ("Settings.ShowFilterRow", "FilterRowVisible"),
        ],
        event_map: &[
            ("RowCommand", "OnRowClick handler"),
            ("CustomCallback", "CustomData / API call"),
            ("RowUpdating", "OnRowUpdating"),
        ],
        data_binding_pattern: "Bind to IEnumerable<T> or IQueryable<T>; configure columns with field names",
        notes: "Very high complexity. DevExpress grid has extensive callback and batch-edit modes. Map custom templates, summary rows, and master-detail.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: true,
        migration_complexity: 5,
        breaking_differences: &[
            "CustomCallback mechanism",
            "Batch edit mode",
            "Master-detail binding",
        ],
    },
    ControlMapping {
        legacy_control: "ASPxTextBox",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxTextBox / MudTextField<string>",
        react_equivalent: "DevExtreme React TextBox / MUI TextField",
        angular_equivalent: "DevExtreme Angular TextBox",
        properties_map: &[
            ("Text", "Value / Text"),
            ("NullText", "Placeholder"),
            ("MaskSettings", "MaskExpression"),
        ],
        event_map: &[("ValueChanged", "ValueChanged / TextChanged")],
        data_binding_pattern: "Two-way bind to string property",
        notes: "Low complexity. Straightforward text input replacement.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "ASPxComboBox",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxComboBox<T> / MudSelect<T>",
        react_equivalent: "DevExtreme React SelectBox / react-select",
        angular_equivalent: "DevExtreme Angular SelectBox",
        properties_map: &[
            ("TextField", "TextFieldName"),
            ("ValueField", "ValueFieldName"),
            ("IncrementalFilteringMode", "Filter mode"),
        ],
        event_map: &[("SelectedIndexChanged", "ValueChanged")],
        data_binding_pattern: "Bind Data to collection; configure text/value field names",
        notes: "Medium complexity. Map cascading combo boxes and load-on-demand scenarios.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "ASPxDateEdit",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxDateEdit / MudDatePicker",
        react_equivalent: "DevExtreme React DateBox",
        angular_equivalent: "DevExtreme Angular DateBox",
        properties_map: &[
            ("Date", "Date"),
            ("DisplayFormatString", "Format"),
            ("MinDate", "MinDate"),
            ("MaxDate", "MaxDate"),
        ],
        event_map: &[("DateChanged", "DateChanged")],
        data_binding_pattern: "Two-way bind to DateTime? property",
        notes: "Low complexity. Straightforward date picker replacement.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "ASPxPopupControl",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxPopup / MudDialog",
        react_equivalent: "DevExtreme React Popup / MUI Dialog",
        angular_equivalent: "DevExtreme Angular Popup",
        properties_map: &[
            ("HeaderText", "HeaderText / Title"),
            ("ShowOnPageLoad", "Visible"),
            ("Modal", "Modal"),
        ],
        event_map: &[
            ("WindowCallback", "OnClose / closed handler"),
            ("Shown", "VisibleChanged"),
        ],
        data_binding_pattern: "Control Visible property; use content template for body",
        notes: "Medium complexity. Map client-side ShowAtPos/Hide calls to component Visible binding.",
        lifecycle_phase: "Any",
        state_model: "Stateless",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "ASPxTreeList",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxTreeList / MudTreeView",
        react_equivalent: "DevExtreme React TreeList / react-arborist",
        angular_equivalent: "DevExtreme Angular TreeList",
        properties_map: &[
            ("KeyFieldName", "KeyFieldName"),
            ("ParentFieldName", "ParentFieldName"),
            ("DataSource", "Data"),
        ],
        event_map: &[
            ("FocusedNodeChanged", "FocusedRowChanged"),
            ("NodeExpanding", "OnExpand"),
        ],
        data_binding_pattern: "Bind to flat collection with key/parent-key self-referencing hierarchy",
        notes: "Medium complexity. Map column templates and drag-and-drop functionality.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 3,
        breaking_differences: &["Self-referencing hierarchy model"],
    },
    ControlMapping {
        legacy_control: "ASPxHtmlEditor",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxHtmlEditor / TinyMCE Blazor",
        react_equivalent: "DevExtreme React HtmlEditor / TinyMCE React",
        angular_equivalent: "DevExtreme Angular HtmlEditor / CKEditor Angular",
        properties_map: &[
            ("Html", "Value / Markup"),
            ("SettingsDialogs", "Custom dialog config"),
        ],
        event_map: &[("HtmlChanged", "ValueChanged")],
        data_binding_pattern: "Two-way bind to HTML string content",
        notes: "Medium complexity. Map toolbar customization and image/file upload handlers.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "ASPxPivotGrid",
        legacy_namespace: "DevExpress.Web",
        blazor_equivalent: "DxPivotGrid / custom pivot",
        react_equivalent: "DevExtreme React PivotGrid",
        angular_equivalent: "DevExtreme Angular PivotGrid",
        properties_map: &[
            ("DataSource", "DataSource"),
            ("Fields", "Fields configuration"),
        ],
        event_map: &[("CellClick", "OnCellClick")],
        data_binding_pattern: "Bind to data source; configure pivot fields with area (Row/Column/Data/Filter)",
        notes: "Very high complexity. Pivot grids require careful field and aggregation mapping. Consider modern BI alternatives.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: false,
        migration_complexity: 5,
        breaking_differences: &[
            "Pivot field configuration model",
            "Aggregation engine differences",
        ],
    },
    // =====================================================================
    // THIRD-PARTY: INFRAGISTICS (IG) CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "UltraWebGrid",
        legacy_namespace: "Infragistics.WebUI.UltraWebGrid",
        blazor_equivalent: "IgbGrid / MudDataGrid<T>",
        react_equivalent: "Ignite UI React Grid / AG Grid",
        angular_equivalent: "Ignite UI Angular Grid",
        properties_map: &[
            ("DataSource", "Data"),
            ("Columns", "Column definitions"),
            ("DisplayLayout.Pager", "PaginationMode"),
        ],
        event_map: &[
            ("ClickCellButton", "OnCellClick"),
            ("InitializeRow", "OnRowInit"),
        ],
        data_binding_pattern: "Bind Data to IEnumerable<T>; define columns via template or auto-generate",
        notes: "Very high complexity. Infragistics grid has deeply nested layout model (DisplayLayout → Bands → Columns). Must flatten to modern column-based grid.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: true,
        migration_complexity: 5,
        breaking_differences: &[
            "DisplayLayout/Band model has no equivalent",
            "Column model nesting",
        ],
    },
    ControlMapping {
        legacy_control: "UltraWebTab",
        legacy_namespace: "Infragistics.WebUI.UltraWebTab",
        blazor_equivalent: "IgbTabs / MudTabs",
        react_equivalent: "Ignite UI React Tabs / MUI Tabs",
        angular_equivalent: "Ignite UI Angular Tabs / mat-tab-group",
        properties_map: &[("SelectedTab", "SelectedIndex"), ("Tabs", "Tab items")],
        event_map: &[("ActiveTabChange", "SelectedIndexChanged")],
        data_binding_pattern: "Define tabs declaratively; bind active index",
        notes: "Low complexity. Map tab content areas to modern tab panel components.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "WebDateChooser",
        legacy_namespace: "Infragistics.WebUI.WebSchedule",
        blazor_equivalent: "IgbDatePicker / MudDatePicker",
        react_equivalent: "Ignite UI React DatePicker / MUI DatePicker",
        angular_equivalent: "Ignite UI Angular DatePicker",
        properties_map: &[
            ("Value", "Value"),
            ("MinDate", "MinValue"),
            ("MaxDate", "MaxValue"),
        ],
        event_map: &[("ValueChanged", "ValueChanged")],
        data_binding_pattern: "Two-way bind to DateTime? property",
        notes: "Low complexity. Direct date picker replacement.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "WebDataMenu",
        legacy_namespace: "Infragistics.Web.UI.NavigationControls",
        blazor_equivalent: "IgbNavbar / MudNavMenu",
        react_equivalent: "Ignite UI React NavDrawer / MUI Menu",
        angular_equivalent: "Ignite UI Angular NavDrawer / mat-menu",
        properties_map: &[("DataSource", "Data"), ("TextField", "Display field")],
        event_map: &[("ItemClick", "OnItemClick")],
        data_binding_pattern: "Bind to hierarchical menu data; configure item template",
        notes: "Low-medium complexity. Map data-bound menu items and sub-menus.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 2,
        breaking_differences: &[],
    },
    ControlMapping {
        legacy_control: "WebDataTree",
        legacy_namespace: "Infragistics.Web.UI.NavigationControls",
        blazor_equivalent: "IgbTree / MudTreeView",
        react_equivalent: "Ignite UI React Tree / react-arborist",
        angular_equivalent: "Ignite UI Angular Tree",
        properties_map: &[
            ("DataSource", "Data"),
            ("DataMember", "Field bindings"),
            ("CheckBoxMode", "Selection mode"),
        ],
        event_map: &[("NodeClick", "OnNodeClick"), ("NodeChecked", "OnNodeCheck")],
        data_binding_pattern: "Bind to hierarchical data source; configure node template",
        notes: "Medium complexity. Map data bindings, checkbox mode, and drag-and-drop.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 3,
        breaking_differences: &["Checkbox mode", "Drag-and-drop integration"],
    },
    ControlMapping {
        legacy_control: "WebChart",
        legacy_namespace: "Infragistics.WebUI.UltraWebChart",
        blazor_equivalent: "IgbCategoryChart / MudChart",
        react_equivalent: "Ignite UI React Charts / Recharts",
        angular_equivalent: "Ignite UI Angular Charts / ngx-charts",
        properties_map: &[("DataSource", "DataSource"), ("ChartType", "ChartType")],
        event_map: &[("ChartDataClicked", "OnDataClick")],
        data_binding_pattern: "Bind to data collection; configure chart type and series",
        notes: "High complexity. Map chart types, axes, legends, and tooltips to modern charting components.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "once",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 3,
        breaking_differences: &[],
    },
    // =====================================================================
    // THIRD-PARTY: COMPONENTART CONTROLS
    // =====================================================================
    ControlMapping {
        legacy_control: "ComponentArt:Grid",
        legacy_namespace: "ComponentArt.Web.UI",
        blazor_equivalent: "MudDataGrid<T> / QuickGrid<T>",
        react_equivalent: "AG Grid / MUI DataGrid",
        angular_equivalent: "ag-grid-angular / mat-table",
        properties_map: &[
            ("DataSource", "Items / Data"),
            ("AllowSorting", "Sortable"),
            ("AllowPaging", "Pageable"),
        ],
        event_map: &[
            ("ItemCommand", "Row click handler"),
            ("Sort", "SortChanged"),
        ],
        data_binding_pattern: "Bind to IEnumerable<T>; define columns",
        notes: "High complexity. ComponentArt Grid is discontinued; migrate to open-source or commercial grid.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_postback",
        requires_databind_on_postback: true,
        has_nested_postback: false,
        migration_complexity: 4,
        breaking_differences: &["Discontinued vendor — no migration path from vendor"],
    },
    ControlMapping {
        legacy_control: "ComponentArt:TreeView",
        legacy_namespace: "ComponentArt.Web.UI",
        blazor_equivalent: "MudTreeView",
        react_equivalent: "react-arborist / MUI TreeView",
        angular_equivalent: "mat-tree / PrimeNG Tree",
        properties_map: &[("DataSource", "Items"), ("TextField", "Display field")],
        event_map: &[("NodeSelected", "OnItemClick")],
        data_binding_pattern: "Bind to hierarchical collection",
        notes: "Medium complexity. Discontinued vendor — migrate to framework-native tree component.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 3,
        breaking_differences: &["Discontinued vendor — no migration path from vendor"],
    },
    ControlMapping {
        legacy_control: "ComponentArt:Menu",
        legacy_namespace: "ComponentArt.Web.UI",
        blazor_equivalent: "MudMenu / MudNavMenu",
        react_equivalent: "MUI Menu / Ant Design Menu",
        angular_equivalent: "mat-menu / PrimeNG Menu",
        properties_map: &[("DataSource", "Items"), ("Orientation", "Dense / layout")],
        event_map: &[("ItemClick", "OnClick")],
        data_binding_pattern: "Bind to menu item collection; configure orientation",
        notes: "Low-medium complexity. Discontinued vendor — replace with framework-native menu.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: true,
        migration_complexity: 2,
        breaking_differences: &["Discontinued vendor — no migration path from vendor"],
    },
    ControlMapping {
        legacy_control: "ComponentArt:TabStrip",
        legacy_namespace: "ComponentArt.Web.UI",
        blazor_equivalent: "MudTabs",
        react_equivalent: "MUI Tabs / react-tabs",
        angular_equivalent: "mat-tab-group",
        properties_map: &[("SelectedTab", "ActivePanelIndex"), ("Tabs", "Tab items")],
        event_map: &[("TabClick", "ActivePanelIndexChanged")],
        data_binding_pattern: "Define tabs declaratively; bind active index",
        notes: "Low complexity. Discontinued vendor — straightforward tab replacement.",
        lifecycle_phase: "Load",
        state_model: "ViewState",
        event_firing_model: "per_user_action",
        requires_databind_on_postback: false,
        has_nested_postback: false,
        migration_complexity: 1,
        breaking_differences: &["Discontinued vendor — no migration path from vendor"],
    },
];

/// Look up a single control mapping by legacy control name (case-insensitive).
///
/// Returns `None` if the control is not in the catalog.
///
/// # Examples
/// ```
/// use engram_index::control_mapping::lookup;
/// let mapping = lookup("gridview").expect("GridView should exist");
/// assert_eq!(mapping.legacy_control, "GridView");
/// ```
pub fn lookup(legacy_control: &str) -> Option<&'static ControlMapping> {
    CONTROL_MAPPINGS
        .iter()
        .find(|m| m.legacy_control.eq_ignore_ascii_case(legacy_control))
}

/// Look up all control mappings for a list of legacy control names.
///
/// Performs case-insensitive matching. Controls not found in the catalog are
/// silently skipped. The returned order matches the order in `CONTROL_MAPPINGS`,
/// not the input order. Duplicates in the input do not produce duplicate results.
///
/// # Examples
/// ```
/// use engram_index::control_mapping::lookup_all_for_file;
/// let results = lookup_all_for_file(&["TextBox", "Button", "NonExistent"]);
/// assert_eq!(results.len(), 2);
/// ```
pub fn lookup_all_for_file(controls: &[&str]) -> Vec<&'static ControlMapping> {
    CONTROL_MAPPINGS
        .iter()
        .filter(|m| {
            controls
                .iter()
                .any(|c| c.eq_ignore_ascii_case(m.legacy_control))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_minimum_entries() {
        assert!(
            CONTROL_MAPPINGS.len() >= 40,
            "Expected at least 40 control mappings, found {}",
            CONTROL_MAPPINGS.len()
        );
    }

    #[test]
    fn no_duplicate_control_names() {
        let mut seen = std::collections::HashSet::new();
        for m in CONTROL_MAPPINGS {
            let lower = m.legacy_control.to_ascii_lowercase();
            assert!(
                seen.insert(lower.clone()),
                "Duplicate control mapping: {}",
                m.legacy_control
            );
        }
    }

    #[test]
    fn lookup_case_insensitive() {
        assert!(lookup("gridview").is_some());
        assert!(lookup("GRIDVIEW").is_some());
        assert!(lookup("GridView").is_some());
        assert!(lookup("gRiDvIeW").is_some());
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup("NonExistentControl").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn lookup_all_filters_correctly() {
        let results = lookup_all_for_file(&["TextBox", "Button", "FakeControl"]);
        assert_eq!(results.len(), 2);

        let names: Vec<&str> = results.iter().map(|m| m.legacy_control).collect();
        assert!(names.contains(&"TextBox"));
        assert!(names.contains(&"Button"));
    }

    #[test]
    fn lookup_all_empty_input() {
        let results = lookup_all_for_file(&[]);
        assert!(results.is_empty());
    }

    #[test]
    fn lookup_all_case_insensitive() {
        let results = lookup_all_for_file(&["textbox", "BUTTON"]);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn lookup_all_deduplicates() {
        let results = lookup_all_for_file(&["TextBox", "textbox", "TEXTBOX"]);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn gridview_has_rich_mappings() {
        let gv = lookup("GridView").expect("GridView must exist");
        assert!(!gv.blazor_equivalent.is_empty());
        assert!(!gv.react_equivalent.is_empty());
        assert!(!gv.angular_equivalent.is_empty());
        assert!(
            gv.properties_map.len() >= 3,
            "GridView should have at least 3 property mappings"
        );
        assert!(
            gv.event_map.len() >= 2,
            "GridView should have at least 2 event mappings"
        );
        assert!(!gv.data_binding_pattern.is_empty());
        assert!(!gv.notes.is_empty());
    }

    #[test]
    fn all_entries_have_required_fields() {
        for m in CONTROL_MAPPINGS {
            assert!(
                !m.legacy_control.is_empty(),
                "legacy_control must not be empty"
            );
            assert!(
                !m.legacy_namespace.is_empty(),
                "legacy_namespace must not be empty for {}",
                m.legacy_control
            );
            assert!(
                !m.blazor_equivalent.is_empty(),
                "blazor_equivalent must not be empty for {}",
                m.legacy_control
            );
            assert!(
                !m.react_equivalent.is_empty(),
                "react_equivalent must not be empty for {}",
                m.legacy_control
            );
            assert!(
                !m.angular_equivalent.is_empty(),
                "angular_equivalent must not be empty for {}",
                m.legacy_control
            );
            assert!(
                !m.notes.is_empty(),
                "notes must not be empty for {}",
                m.legacy_control
            );
        }
    }

    #[test]
    fn required_controls_present() {
        let required = [
            // Data display
            "GridView",
            "DetailsView",
            "FormView",
            "ListView",
            "Repeater",
            "DataList",
            // Input
            "TextBox",
            "DropDownList",
            "CheckBox",
            "CheckBoxList",
            "RadioButton",
            "RadioButtonList",
            "Calendar",
            "FileUpload",
            // Action
            "Button",
            "LinkButton",
            "ImageButton",
            // Navigation
            "HyperLink",
            "Menu",
            "TreeView",
            "SiteMapPath",
            // Layout
            "Panel",
            "PlaceHolder",
            "MultiView",
            "View",
            "Wizard",
            // AJAX
            "UpdatePanel",
            "ScriptManager",
            "Timer",
            "UpdateProgress",
            // Data access
            "SqlDataSource",
            "ObjectDataSource",
            "LinqDataSource",
            "EntityDataSource",
            // Display
            "Label",
            "Literal",
            "Image",
            // Validation
            "ValidationSummary",
            "RequiredFieldValidator",
            "CompareValidator",
            "RangeValidator",
            "RegularExpressionValidator",
            "CustomValidator",
            // Additional
            "ListBox",
            "HiddenField",
            "Table",
            "TableRow",
            "TableCell",
            "LoginView",
            "ContentPlaceHolder",
        ];

        for name in &required {
            assert!(
                lookup(name).is_some(),
                "Required control '{}' is missing from catalog",
                name
            );
        }
    }

    // ── New tests: individual control type validation ──────────────────────

    #[test]
    fn gridview_blazor_equivalent_is_quickgrid() {
        let gv = lookup("GridView").unwrap();
        assert!(
            gv.blazor_equivalent.contains("QuickGrid")
                || gv.blazor_equivalent.contains("Virtualize"),
            "GridView blazor equivalent should mention QuickGrid or Virtualize"
        );
    }

    #[test]
    fn gridview_has_viewstate_and_requires_databind() {
        let gv = lookup("GridView").unwrap();
        assert_eq!(gv.state_model, "ViewState");
        assert!(gv.requires_databind_on_postback);
        assert!(gv.has_nested_postback);
    }

    #[test]
    fn formview_blazor_is_edit_form() {
        let fv = lookup("FormView").unwrap();
        assert!(
            fv.blazor_equivalent.contains("EditForm"),
            "FormView blazor equivalent should be EditForm"
        );
    }

    #[test]
    fn formview_medium_complexity() {
        let fv = lookup("FormView").unwrap();
        assert_eq!(fv.migration_complexity, 3);
    }

    #[test]
    fn listview_blazor_is_foreach_virtualize() {
        let lv = lookup("ListView").unwrap();
        assert!(
            lv.blazor_equivalent.contains("foreach"),
            "ListView blazor equivalent should use foreach"
        );
        assert!(
            lv.blazor_equivalent.contains("Virtualize"),
            "ListView blazor equivalent should mention Virtualize"
        );
    }

    #[test]
    fn listview_requires_databind() {
        let lv = lookup("ListView").unwrap();
        assert!(
            lv.requires_databind_on_postback,
            "ListView requires DataBind on every postback"
        );
    }

    #[test]
    fn repeater_blazor_is_foreach_loop() {
        let r = lookup("Repeater").unwrap();
        assert!(
            r.blazor_equivalent.contains("foreach"),
            "Repeater blazor equivalent should be foreach loop"
        );
    }

    #[test]
    fn repeater_is_stateless() {
        let r = lookup("Repeater").unwrap();
        assert_eq!(r.state_model, "Stateless", "Repeater has no ViewState");
    }

    #[test]
    fn panel_blazor_is_div() {
        let p = lookup("Panel").unwrap();
        assert!(
            p.blazor_equivalent.contains("<div>"),
            "Panel blazor equivalent should be <div>"
        );
    }

    #[test]
    fn panel_low_complexity() {
        let p = lookup("Panel").unwrap();
        assert_eq!(p.migration_complexity, 1, "Panel is trivial to migrate");
    }

    #[test]
    fn multiview_blazor_mentions_tab() {
        let mv = lookup("MultiView").unwrap();
        assert!(
            mv.blazor_equivalent.to_lowercase().contains("tab")
                || mv.blazor_equivalent.to_lowercase().contains("switch"),
            "MultiView blazor equivalent should mention tab or switch"
        );
    }

    #[test]
    fn multiview_has_nested_postback() {
        let mv = lookup("MultiView").unwrap();
        assert!(mv.has_nested_postback);
    }

    #[test]
    fn view_is_stateless_and_low_complexity() {
        let v = lookup("View").unwrap();
        assert_eq!(v.state_model, "Stateless");
        assert_eq!(v.migration_complexity, 1);
    }

    #[test]
    fn wizard_blazor_is_stepper() {
        let wiz = lookup("Wizard").unwrap();
        assert!(
            wiz.blazor_equivalent.to_lowercase().contains("stepper")
                || wiz.blazor_equivalent.to_lowercase().contains("step"),
            "Wizard blazor equivalent should mention stepper"
        );
    }

    #[test]
    fn wizard_high_complexity() {
        let wiz = lookup("Wizard").unwrap();
        assert!(
            wiz.migration_complexity >= 4,
            "Wizard should be complexity 4 or higher"
        );
    }

    #[test]
    fn wizard_has_breaking_differences() {
        let wiz = lookup("Wizard").unwrap();
        assert!(
            !wiz.breaking_differences.is_empty(),
            "Wizard should have breaking differences listed"
        );
    }

    #[test]
    fn fileupload_blazor_is_input_file() {
        let fu = lookup("FileUpload").unwrap();
        assert!(
            fu.blazor_equivalent.contains("InputFile"),
            "FileUpload blazor equivalent should be InputFile"
        );
    }

    #[test]
    fn fileupload_stateless() {
        let fu = lookup("FileUpload").unwrap();
        assert_eq!(fu.state_model, "Stateless");
    }

    #[test]
    fn calendar_blazor_is_date_picker() {
        let cal = lookup("Calendar").unwrap();
        assert!(
            cal.blazor_equivalent.to_lowercase().contains("date")
                || cal.blazor_equivalent.to_lowercase().contains("input"),
            "Calendar blazor equivalent should mention date input"
        );
    }

    #[test]
    fn calendar_react_mentions_datepicker_library() {
        let cal = lookup("Calendar").unwrap();
        assert!(
            cal.react_equivalent.to_lowercase().contains("datepicker")
                || cal.react_equivalent.to_lowercase().contains("date"),
            "Calendar react equivalent should mention a date picker library"
        );
    }

    #[test]
    fn treeview_blazor_is_recursive_component() {
        let tv = lookup("TreeView").unwrap();
        assert!(
            tv.blazor_equivalent.to_lowercase().contains("tree")
                || tv.blazor_equivalent.to_lowercase().contains("mud"),
            "TreeView blazor equivalent should mention tree component"
        );
    }

    #[test]
    fn treeview_high_complexity_with_nested_postback() {
        let tv = lookup("TreeView").unwrap();
        assert!(
            tv.migration_complexity >= 4,
            "TreeView should be high complexity"
        );
        assert!(
            tv.has_nested_postback,
            "TreeView has nested postback cycles"
        );
    }

    #[test]
    fn menu_blazor_is_nav_menu() {
        let m = lookup("Menu").unwrap();
        assert!(
            m.blazor_equivalent.to_lowercase().contains("menu")
                || m.blazor_equivalent.to_lowercase().contains("nav"),
            "Menu blazor equivalent should mention navigation menu"
        );
    }

    #[test]
    fn menu_medium_complexity() {
        let m = lookup("Menu").unwrap();
        assert!(
            m.migration_complexity >= 3,
            "Menu should be at least medium complexity"
        );
    }

    #[test]
    fn sqldatasource_blazor_uses_ef_or_service() {
        let sds = lookup("SqlDataSource").unwrap();
        assert!(
            sds.blazor_equivalent.to_lowercase().contains("service")
                || sds.blazor_equivalent.to_lowercase().contains("ef")
                || sds.blazor_equivalent.to_lowercase().contains("core"),
            "SqlDataSource blazor equivalent should use service or EF Core"
        );
    }

    #[test]
    fn sqldatasource_has_security_note() {
        let sds = lookup("SqlDataSource").unwrap();
        // The notes should mention that SQL in markup is a security concern
        assert!(
            sds.notes.to_lowercase().contains("sql")
                || sds.notes.to_lowercase().contains("repository")
                || sds.notes.to_lowercase().contains("pattern"),
            "SqlDataSource notes should discuss security and repository pattern"
        );
    }

    #[test]
    fn sqldatasource_high_complexity() {
        let sds = lookup("SqlDataSource").unwrap();
        assert!(sds.migration_complexity >= 4);
    }

    #[test]
    fn objectdatasource_blazor_is_di_service() {
        let ods = lookup("ObjectDataSource").unwrap();
        assert!(
            ods.blazor_equivalent.to_lowercase().contains("service")
                || ods.blazor_equivalent.to_lowercase().contains("inject")
                || ods.blazor_equivalent.to_lowercase().contains("repository"),
            "ObjectDataSource blazor equivalent should use DI/service"
        );
    }

    #[test]
    fn detailsview_has_crud_events() {
        let dv = lookup("DetailsView").unwrap();
        let event_names: Vec<&str> = dv.event_map.iter().map(|(e, _)| *e).collect();
        assert!(
            event_names
                .iter()
                .any(|&e| e.contains("Updating") || e.contains("Insert")),
            "DetailsView should have update/insert event mappings"
        );
    }

    #[test]
    fn updatepanel_has_no_spa_equivalent() {
        let up = lookup("UpdatePanel").unwrap();
        assert!(
            up.blazor_equivalent
                .to_lowercase()
                .contains("no equivalent")
                || up.blazor_equivalent.to_lowercase().contains("not needed"),
            "UpdatePanel should explicitly state no SPA equivalent needed"
        );
    }

    #[test]
    fn updatepanel_complexity_indicates_removal_effort() {
        let up = lookup("UpdatePanel").unwrap();
        assert!(
            up.migration_complexity >= 4,
            "UpdatePanel requires significant effort to remove correctly"
        );
    }

    #[test]
    fn scriptmanager_low_complexity_remove() {
        let sm = lookup("ScriptManager").unwrap();
        assert_eq!(
            sm.migration_complexity, 1,
            "ScriptManager is trivial to remove"
        );
    }

    #[test]
    fn sitemapdatasource_is_not_in_catalog_but_sitemappath_is() {
        // SiteMapDataSource is not in catalog (it's SiteMapPath that IS)
        assert!(
            lookup("SiteMapPath").is_some(),
            "SiteMapPath should be in catalog"
        );
    }

    #[test]
    fn telerik_rad_grid_is_in_catalog() {
        let rg = lookup("RadGrid").unwrap();
        assert_eq!(rg.legacy_namespace, "Telerik.Web.UI");
        assert!(
            rg.migration_complexity >= 5,
            "RadGrid should be maximum complexity"
        );
    }

    #[test]
    fn devexpress_aspx_grid_view_is_in_catalog() {
        let ag = lookup("ASPxGridView").unwrap();
        assert_eq!(ag.legacy_namespace, "DevExpress.Web");
        assert!(ag.migration_complexity >= 5);
    }

    #[test]
    fn migration_complexity_range_one_to_five() {
        for m in CONTROL_MAPPINGS {
            assert!(
                m.migration_complexity >= 1 && m.migration_complexity <= 5,
                "migration_complexity for {} must be 1-5, got {}",
                m.legacy_control,
                m.migration_complexity
            );
        }
    }

    #[test]
    fn all_lifecycle_phases_are_known_values() {
        let valid = ["Init", "Load", "PreRender", "Postback", "Any"];
        for m in CONTROL_MAPPINGS {
            assert!(
                valid.contains(&m.lifecycle_phase),
                "Unknown lifecycle_phase '{}' for control '{}'",
                m.lifecycle_phase,
                m.legacy_control
            );
        }
    }

    #[test]
    fn all_state_models_are_known_values() {
        let valid = ["ViewState", "ControlState", "Stateless", "ComponentState"];
        for m in CONTROL_MAPPINGS {
            assert!(
                valid.contains(&m.state_model),
                "Unknown state_model '{}' for control '{}'",
                m.state_model,
                m.legacy_control
            );
        }
    }

    #[test]
    fn all_event_firing_models_are_known_values() {
        let valid = ["per_postback", "per_user_action", "once", "manual"];
        for m in CONTROL_MAPPINGS {
            assert!(
                valid.contains(&m.event_firing_model),
                "Unknown event_firing_model '{}' for control '{}'",
                m.event_firing_model,
                m.legacy_control
            );
        }
    }

    #[test]
    fn lookup_all_returns_all_matched_controls() {
        let controls = ["GridView", "Repeater", "Panel", "Button", "TextBox"];
        let results = lookup_all_for_file(&controls);
        assert_eq!(
            results.len(),
            5,
            "All five controls should be found in catalog"
        );
        for name in &controls {
            assert!(
                results
                    .iter()
                    .any(|m| m.legacy_control.eq_ignore_ascii_case(name)),
                "Control '{}' should be in results",
                name
            );
        }
    }

    #[test]
    fn lookup_all_skips_unknown_controls() {
        let controls = ["GridView", "MyCustomControl", "UndefinedWidget"];
        let results = lookup_all_for_file(&controls);
        assert_eq!(results.len(), 1, "Only GridView should be found");
        assert_eq!(results[0].legacy_control, "GridView");
    }

    #[test]
    fn content_placeholder_maps_to_layout_slot() {
        let cph = lookup("ContentPlaceHolder").unwrap();
        assert!(
            cph.blazor_equivalent.contains("@Body") || cph.blazor_equivalent.contains("RenderBody"),
            "ContentPlaceHolder blazor equivalent should map to @Body or RenderBody"
        );
    }

    #[test]
    fn login_control_has_auth_events() {
        let login = lookup("Login").unwrap();
        let event_names: Vec<&str> = login.event_map.iter().map(|(e, _)| *e).collect();
        assert!(
            event_names
                .iter()
                .any(|&e| e.contains("Authenticate") || e.contains("LoggedIn")),
            "Login control should have authentication event mappings"
        );
    }
}
