package admin

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"time"

	"github.com/go-chi/chi/v5"
	"github.com/go-chi/chi/v5/middleware"
	"go.uber.org/zap"

	"github.com/your-org/stateform/control-plane/internal/config"
	grpcsrv "github.com/your-org/stateform/control-plane/internal/grpc"
)

// ── API server ────────────────────────────────────────────────────────────────

// Server is the Admin REST API — the management surface for operators and
// the future management UI.
//
// ## Endpoints
//
//   POST   /api/v1/reload              — hot-reload config from disk + broadcast
//   GET    /api/v1/config              — dump current effective config
//   GET    /api/v1/routes              — list all routes
//   GET    /api/v1/routes/{id}         — get one route
//   GET    /api/v1/upstreams           — list all upstreams
//   GET    /api/v1/upstreams/{name}    — get one upstream
//   GET    /api/v1/status              — gateway health + subscriber count
//   GET    /healthz                    — liveness probe
//
// ## Future (management UI surface)
//
//   PUT    /api/v1/routes/{id}         — update a route (writes YAML, then reloads)
//   DELETE /api/v1/routes/{id}         — remove a route
//   PUT    /api/v1/upstreams/{name}    — update an upstream
//   POST   /api/v1/api-keys            — provision an API key (writes to Redis)
//   DELETE /api/v1/api-keys/{key}      — revoke an API key
type Server struct {
	loader  *config.Loader
	grpc    *grpcsrv.Server
	log     *zap.Logger
	router  http.Handler
}

func NewServer(loader *config.Loader, grpc *grpcsrv.Server, log *zap.Logger) *Server {
	s := &Server{loader: loader, grpc: grpc, log: log}
	s.router = s.buildRouter()
	return s
}

func (s *Server) buildRouter() http.Handler {
	r := chi.NewRouter()

	r.Use(middleware.RequestID)
	r.Use(middleware.RealIP)
	r.Use(zapMiddleware(s.log))
	r.Use(middleware.Recoverer)

	// Liveness probe — always 200 regardless of config state
	r.Get("/healthz", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
	})

	r.Route("/api/v1", func(r chi.Router) {
		// ── Reload ────────────────────────────────────────────────────────────
		r.Post("/reload", s.handleReload)

		// ── Config ────────────────────────────────────────────────────────────
		r.Get("/config", s.handleGetConfig)

		// ── Routes ────────────────────────────────────────────────────────────
		r.Get("/routes", s.handleListRoutes)
		r.Get("/routes/{id}", s.handleGetRoute)

		// ── Upstreams ─────────────────────────────────────────────────────────
		r.Get("/upstreams", s.handleListUpstreams)
		r.Get("/upstreams/{name}", s.handleGetUpstream)

		// ── Status ────────────────────────────────────────────────────────────
		r.Get("/status", s.handleStatus)
	})

	return r
}

// Listen starts the HTTP server on addr and blocks until ctx is cancelled.
func (s *Server) Listen(ctx context.Context, addr string) error {
	lis, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}

	srv := &http.Server{
		Handler:      s.router,
		ReadTimeout:  10 * time.Second,
		WriteTimeout: 30 * time.Second,
		IdleTimeout:  90 * time.Second,
	}

	s.log.Info("admin API listening", zap.String("addr", addr))

	go func() {
		<-ctx.Done()
		shutCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		_ = srv.Shutdown(shutCtx)
	}()

	return srv.Serve(lis)
}

// ── Handlers ──────────────────────────────────────────────────────────────────

func (s *Server) handleReload(w http.ResponseWriter, r *http.Request) {
	if err := s.loader.Reload(); err != nil {
		writeError(w, http.StatusInternalServerError, "reload failed: "+err.Error())
		return
	}

	_, rawYAML, version := s.loader.Current()
	s.grpc.BroadcastSnapshot(rawYAML, version, "admin api reload")

	writeJSON(w, http.StatusOK, map[string]interface{}{
		"version": version,
		"message": "config reloaded and broadcast to all data plane instances",
	})
}

func (s *Server) handleGetConfig(w http.ResponseWriter, r *http.Request) {
	cfg, _, version := s.loader.Current()
	writeJSON(w, http.StatusOK, map[string]interface{}{
		"version": version,
		"config":  cfg,
	})
}

func (s *Server) handleListRoutes(w http.ResponseWriter, r *http.Request) {
	cfg, _, version := s.loader.Current()
	writeJSON(w, http.StatusOK, map[string]interface{}{
		"version": version,
		"count":   len(cfg.Routes),
		"routes":  cfg.Routes,
	})
}

func (s *Server) handleGetRoute(w http.ResponseWriter, r *http.Request) {
	id := chi.URLParam(r, "id")
	cfg, _, _ := s.loader.Current()

	for _, route := range cfg.Routes {
		if route.ID == id {
			writeJSON(w, http.StatusOK, route)
			return
		}
	}

	writeError(w, http.StatusNotFound, "route not found: "+id)
}

func (s *Server) handleListUpstreams(w http.ResponseWriter, r *http.Request) {
	cfg, _, version := s.loader.Current()
	writeJSON(w, http.StatusOK, map[string]interface{}{
		"version":   version,
		"count":     len(cfg.Upstreams),
		"upstreams": cfg.Upstreams,
	})
}

func (s *Server) handleGetUpstream(w http.ResponseWriter, r *http.Request) {
	name := chi.URLParam(r, "name")
	cfg, _, _ := s.loader.Current()

	upstream, ok := cfg.Upstreams[name]
	if !ok {
		writeError(w, http.StatusNotFound, "upstream not found: "+name)
		return
	}

	writeJSON(w, http.StatusOK, map[string]interface{}{
		"name":     name,
		"upstream": upstream,
	})
}

func (s *Server) handleStatus(w http.ResponseWriter, r *http.Request) {
	_, _, version := s.loader.Current()
	writeJSON(w, http.StatusOK, map[string]interface{}{
		"config_version":     version,
		"active_subscribers": s.grpc.ActiveSubscribers(),
		"uptime":             time.Since(startTime).String(),
	})
}

var startTime = time.Now()

// ── Helpers ───────────────────────────────────────────────────────────────────

func writeJSON(w http.ResponseWriter, status int, v interface{}) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeError(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}

// zapMiddleware logs each request at INFO level with method, path, status, and latency.
func zapMiddleware(log *zap.Logger) func(http.Handler) http.Handler {
	return func(next http.Handler) http.Handler {
		return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			start := time.Now()
			ww := middleware.NewWrapResponseWriter(w, r.ProtoMajor)
			next.ServeHTTP(ww, r)
			log.Info("admin api request",
				zap.String("method", r.Method),
				zap.String("path", r.URL.Path),
				zap.Int("status", ww.Status()),
				zap.Duration("latency", time.Since(start)),
				zap.String("request_id", middleware.GetReqID(r.Context())),
			)
		})
	}
}
