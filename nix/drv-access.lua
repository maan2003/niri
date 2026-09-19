-- The apps' side of PipeWire. Clients that came in through the apps' socket carry
-- pipewire.access = "drv-app" (set by the daemon from the socket, not by the client) and
-- get this permission manager: see and play, nothing more. Capture is decided per app and
-- kind ("mic" for anything audio, sink monitors included; "camera" for video) by grants
-- the bridge writes into its "drv-access" metadata (key grant:<uid>, value the kinds);
-- a capture stream with no grant waits unlinked while request:<uid>:<kind> asks the
-- bridge, and is destroyed if the answer is no. Linking is ours alone: apps cannot see the link
-- factory, and PipeWire refuses a link whose owner cannot see the other node.
log = Log.open_topic ("s-drv-access")
lutils = require ("linking-utils")

-- uid -> { mic = true, camera = true }.
grants = {}
-- node bound-id -> owning client bound-id, for the ports.
node_owner = {}

ACCESS = "drv-app"
METADATA = "drv-access"

function granted (uid, kind)
  local g = grants[uid]
  return g ~= nil and g[kind] == true
end

function parse_kinds (value)
  local kinds = {}
  for kind in (value or ""):gmatch ("%S+") do
    kinds[kind] = true
  end
  return kinds
end

-- A capture stream's kind, from its own class ("Stream/Input/Video") or its target's.
function kind_of (media_class)
  if (media_class or ""):find ("Video") then
    return "camera"
  end
  return "mic"
end

-- The apps' clients, looked up live: a removed client is simply gone.
clients_om = ObjectManager {
  Interest { type = "client", Constraint { "pipewire.access", "=", ACCESS } },
}
clients_om:activate ()

function uid_of (client)
  if client.properties["pipewire.access"] ~= ACCESS then
    return nil
  end
  return tonumber (client.properties["pipewire.sec.uid"])
end

-- The app uid a client bound-id belongs to, if it is an app's.
function client_uid (id)
  if not id then
    return nil
  end
  local client = clients_om:lookup {
    Constraint { "bound-id", "=", id, type = "gobject" },
  }
  return client and uid_of (client)
end

function owner_uid (node_props)
  return client_uid (tonumber (node_props["client.id"]))
end

-- What a remote the bridge handed out is for, if the node's client is one: the bridge
-- marks the client id in the metadata ("camera", or "node:<id>" for a cast). The remote
-- sees only those nodes, but linking is ours, so its streams reach nothing else either.
function remote_of (node_props)
  if not metadata_ready then
    return nil
  end
  return metadata:find (tonumber (node_props["client.id"]) or 0, "drv.remote")
end

-- The bridge writes grants here, and reads requests; WirePlumber holds it so that a
-- bridge restart loses nothing and a WirePlumber restart makes the bridge write again.
metadata = ImplMetadata (METADATA)
metadata_ready = false
metadata:activate (Features.ALL, function (m, e)
  if e then
    log:warning ("the " .. METADATA .. " metadata: " .. tostring (e))
  else
    metadata_ready = true
  end
end)

-- Objects an app's client may see. Its own client and nodes; sinks and sources (so it
-- can list them); nothing of another app's; only the factory its streams come through.
-- Nothing by default: a new object is announced to the client only once decided here.
pm = PermissionManager ()
pm:set_default_permissions (Perm.NONE)
-- Without M: metadata (the default sink, say) is set on the core as subject.
pm:set_core_permissions (Perm.RWX)

pm:add_interest_match (function (_, client, obj)
  local props = obj["global-properties"]
  if props["factory.name"] == "client-node" then
    return Perm.RX
  end
  return Perm.NONE
end, Interest { type = "factory" })

pm:add_interest_match (function (_, client, obj)
  local props = obj["global-properties"]
  local owner = tonumber (props["client.id"])
  if owner == client["bound-id"] then
    return Perm.ALL
  end
  local uid = client_uid (owner)
  if uid and uid ~= uid_of (client) then
    return Perm.NONE
  end
  return Perm.RX
end, Interest { type = "node" })

pm:add_interest_match (function (_, client, obj)
  local props = obj["global-properties"]
  local owner = node_owner[tonumber (props["node.id"])]
  if owner == client["bound-id"] then
    return Perm.ALL
  end
  local uid = client_uid (owner)
  if uid and uid ~= uid_of (client) then
    return Perm.NONE
  end
  return Perm.RX
end, Interest { type = "port" })

pm:add_interest_match (function (_, client, obj)
  local props = obj["global-properties"]
  local me = uid_of (client)
  for _, side in ipairs { "link.output.node", "link.input.node" } do
    local uid = client_uid (node_owner[tonumber (props[side])])
    if uid and uid ~= me then
      return Perm.NONE
    end
  end
  return Perm.RX
end, Interest { type = "link" })

pm:add_interest_match (function (_, client, obj)
  if obj["bound-id"] == client["bound-id"] then
    return Perm.RX
  end
  return Perm.NONE
end, Interest { type = "client" })

pm:add_interest_match (function (_, client, obj)
  local props = obj["global-properties"]
  if props["metadata.name"] == METADATA then
    return Perm.NONE
  end
  return Perm.R
end, Interest { type = "metadata" })

SimpleEventHook {
  name = "client/find-drv-access",
  before = "client/find-default-access",
  after = "client/find-config-access",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "select-access" },
    },
  },
  execute = function (event)
    local client = event:get_subject ()
    if client:get_property ("pipewire.access") ~= ACCESS then
      return
    end
    log:info (client, string.format ("app client %d is uid %s", client["bound-id"],
        tostring (client:get_property ("pipewire.sec.uid"))))
    if event:get_data ("permission-manager") == nil then
      event:set_data ("permission-manager", pm)
    end
  end
}:register ()

nodes_om = ObjectManager {
  Interest { type = "node" },
}
nodes_om:connect ("object-added", function (_, node)
  node_owner[node["bound-id"]] = tonumber (node["global-properties"]["client.id"])
  -- Ports and links of this node may have been decided before it was known.
  pm:update_permissions ()
end)
nodes_om:connect ("object-removed", function (_, node)
  node_owner[node["bound-id"]] = nil
end)
nodes_om:activate ()

-- What the bridge still has to answer, so a stream that asks twice asks once.
function request (uid, kind)
  if not metadata_ready then
    log:warning ("no " .. METADATA .. " metadata yet")
    return
  end
  local key = "request:" .. uid .. ":" .. kind
  if metadata:find (0, key) ~= nil then
    return
  end
  log:info (string.format ("uid %d asks for %s", uid, kind))
  metadata:set (0, key, "Spa:String:JSON", "asked")
end

-- The capture stream nodes of `uid` of `kind`.
function capture_streams (source, uid, kind)
  local om = source:call ("get-object-manager", "session-item")
  local found = {}
  for si in om:iterate { type = "SiLinkable" } do
    local props = si.properties
    if props["item.node.type"] == "stream" and props["item.node.direction"] == "input" then
      local node = si:get_associated_proxy ("node")
      if node and owner_uid (node.properties) == uid and kind_of (props["media.class"]) == kind then
        table.insert (found, node)
      end
    end
  end
  return found
end

function destroy_capture (source, uid, kind, why)
  local clients_om = source:call ("get-object-manager", "client")
  for _, node in ipairs (capture_streams (source, uid, kind)) do
    log:info (node, string.format ("uid %d: %s: %s", uid, kind, why))
    local client = clients_om:lookup {
      Constraint { "bound-id", "=", node.properties["client.id"], type = "gobject" }
    }
    if client then
      client:send_error (node["bound-id"], -1, kind .. " " .. why)
    end
    node:request_destroy ()
  end
end

SimpleEventHook {
  name = "linking/drv-capture-gate",
  after = {
    "linking/find-defined-target",
    "linking/find-filter-target",
    "linking/find-media-role-target",
    "linking/find-media-role-sink-target",
    "linking/find-default-target",
  },
  before = "linking/prepare-link",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "select-target" },
    },
  },
  execute = function (event)
    local source, om, si, si_props, si_flags, target =
        lutils:unwrap_select_target_event (event)
    if not target or si_props["item.node.direction"] ~= "input" then
      return
    end
    local node = si:get_associated_proxy ("node")
    local target_node = target:get_associated_proxy ("node")
    local remote = node and remote_of (node.properties)
    if remote then
      local ok
      if remote == "camera" then
        ok = target_node and target_node.properties["media.class"] == "Video/Source"
      else
        ok = target_node and tostring (target_node["bound-id"]) == remote:match ("^node:(%d+)$")
      end
      if not ok then
        log:warning (node, "a remote's stream aimed past its remote (" .. remote .. ")")
        node:request_destroy ()
        event:stop_processing ()
      end
      return
    end
    local uid = node and owner_uid (node.properties)
    if not uid then
      return
    end
    local kind = kind_of (si_props["media.class"])
    if target_node and kind_of (target_node.properties["media.class"]) == "camera" then
      kind = "camera"
    end
    if granted (uid, kind) then
      return
    end
    -- Not linked, not refused: it waits for the answer.
    request (uid, kind)
    event:stop_processing ()
  end
}:register ()

SimpleEventHook {
  name = "linking/drv-access-changed",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "metadata-changed" },
      Constraint { "metadata.name", "=", METADATA },
    },
  },
  execute = function (event)
    local source = event:get_source ()
    local props = event:get_properties ()
    local key, value = props["event.subject.key"], props["event.subject.value"]
    local uid = tonumber ((key or ""):match ("^grant:(%d+)$"))
    if uid then
      local before = grants[uid] or {}
      local now = parse_kinds (value)
      grants[uid] = now
      for kind in pairs (before) do
        if not now[kind] then
          destroy_capture (source, uid, kind, "revoked")
        end
      end
      for kind in pairs (now) do
        if not before[kind] then
          log:info (string.format ("uid %d may use %s", uid, kind))
          source:call ("schedule-rescan", "linking")
        end
      end
      return
    end
    local kind
    uid, kind = (key or ""):match ("^request:(%d+):(%a+)$")
    uid = tonumber (uid)
    -- Answered: the grant, if any, came first.
    if uid and value == nil and not granted (uid, kind) then
      destroy_capture (source, uid, kind, "refused")
    end
  end
}:register ()
