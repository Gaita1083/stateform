package main

import (
	"context"
	"os"
	"os/signal"
	"strings"
	"syscall"

	"go.uber.org/zap"
	"golang.org/x/sync/errgroup"

	"github.com/your-org/stateform/control-plane/internal/admin"
	"github.com/your-org/stateform/control-plane/internal/config"
	grpcsrv "github.com/your-org/stateform/control-plane/internal/grpc"
	"github.com/your-org/stateform/control-plane/internal/k8s"
)

func main() {
	// ── Logger ────────────────────────────────────────────────────────────────
	log, _ := zap.NewProduction()
	defer log.Sync()

	// ── Config ────────────────────────────────────────────────────────────────
	configPath := envOr("STATEFORM_CONFIG", "config.yaml")
	loader, err := config.NewLoader(configPath, log)
	if err != nil {
		log.Fatal("failed to load config", zap.Error(err))
	}
	log.Info("config loaded", zap.String("path", configPath))

	// ── Addresses ─────────────────────────────────────────────────────────────
	grpcAddr  := envOr("STATEFORM_GRPC_ADDR",  ":50051")
	adminAddr := envOr("STATEFORM_ADMIN_ADDR", ":8081")

	// ── Context + signal handling ─────────────────────────────────────────────
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		sig := <-sigs
		log.Info("signal received — shutting down", zap.String("signal", sig.String()))
		cancel()
	}()

	// ── gRPC server ───────────────────────────────────────────────────────────
	grpcServer := grpcsrv.NewServer(loader, log)

	// ── K8s watcher ───────────────────────────────────────────────────────────
	// When the watcher detects a change, it reloads config and broadcasts
	// a new snapshot to all connected data plane instances.
	reloadFn := func(reason string) {
		log.Info("k8s-triggered reload", zap.String("reason", reason))
		if err := loader.Reload(); err != nil {
			log.Warn("reload failed", zap.Error(err))
			return
		}
		_, rawYAML, version := loader.Current()
		grpcServer.BroadcastSnapshot(rawYAML, version, reason)
	}

	namespaces := strings.Split(envOr("STATEFORM_WATCH_NAMESPACES", "default"), ",")
	labelSel   := envOr("STATEFORM_WATCH_LABEL", "")

	watcher, err := k8s.NewWatcher(namespaces, labelSel, reloadFn, log)
	if err != nil {
		log.Fatal("failed to build k8s watcher", zap.Error(err))
	}

	// ── Admin API ─────────────────────────────────────────────────────────────
	adminServer := admin.NewServer(loader, grpcServer, log)

	// ── Run all components concurrently ───────────────────────────────────────
	g, gctx := errgroup.WithContext(ctx)

	g.Go(func() error {
		return grpcsrv.Listen(gctx, grpcAddr, grpcServer, log)
	})

	g.Go(func() error {
		return adminServer.Listen(gctx, adminAddr)
	})

	g.Go(func() error {
		return watcher.Run(gctx)
	})

	if err := g.Wait(); err != nil && err != context.Canceled {
		log.Error("component exited with error", zap.Error(err))
		os.Exit(1)
	}

	log.Info("control plane stopped")
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}
