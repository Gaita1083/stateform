package grpc

import (
	"context"
	"net"
	"sync"
	"time"

	"go.uber.org/zap"
	googlegrpc "google.golang.org/grpc"
	"google.golang.org/grpc/keepalive"

	"github.com/your-org/stateform/control-plane/internal/config"
	pbv1 "github.com/your-org/stateform/control-plane/proto/v1"
)

// ── Subscriber ────────────────────────────────────────────────────────────────

// subscriber represents one connected Rust data plane instance.
type subscriber struct {
	id     string
	stream pbv1.ControlPlaneService_SubscribeServer
	done   chan struct{}
}

// ── Server ────────────────────────────────────────────────────────────────────

// Server implements the ControlPlaneService gRPC service.
//
// ## Fan-out model
//
// When a config snapshot is ready (triggered by K8s events, Admin API calls,
// or the initial subscription), the server fans out the snapshot to every
// connected subscriber concurrently. Each subscriber has its own goroutine
// blocked on stream.Send(); a slow or disconnected subscriber cannot block others.
//
// Subscribers that fail to receive (disconnected data planes) are removed from
// the active set automatically.
type Server struct {
	pbv1.UnimplementedControlPlaneServiceServer

	loader  *config.Loader
	log     *zap.Logger

	mu          sync.RWMutex
	subscribers map[string]*subscriber
}

func NewServer(loader *config.Loader, log *zap.Logger) *Server {
	return &Server{
		loader:      loader,
		log:         log,
		subscribers: make(map[string]*subscriber),
	}
}

// Subscribe is called by each Rust data plane instance on startup.
// It immediately sends the current config snapshot, then streams future
// snapshots as they arrive via BroadcastSnapshot.
func (s *Server) Subscribe(
	req *pbv1.SubscribeRequest,
	stream pbv1.ControlPlaneService_SubscribeServer,
) error {
	s.log.Info("data plane subscribed", zap.String("instance_id", req.InstanceId))

	sub := &subscriber{
		id:     req.InstanceId,
		stream: stream,
		done:   make(chan struct{}),
	}

	s.mu.Lock()
	s.subscribers[req.InstanceId] = sub
	s.mu.Unlock()

	defer func() {
		s.mu.Lock()
		delete(s.subscribers, req.InstanceId)
		s.mu.Unlock()
		s.log.Info("data plane disconnected", zap.String("instance_id", req.InstanceId))
	}()

	// Send the current snapshot immediately so the connecting data plane
	// doesn't have to wait for the next event to get its initial config.
	_, rawYAML, version := s.loader.Current()
	initial := buildSnapshot(rawYAML, version, "startup")
	if err := stream.Send(initial); err != nil {
		return err
	}

	// Block until the subscriber disconnects or the server shuts down
	select {
	case <-sub.done:
	case <-stream.Context().Done():
	}
	return nil
}

// Reload triggers an immediate config re-read and pushes the result to all
// subscribers. Called by the Admin API.
func (s *Server) Reload(
	ctx context.Context,
	req *pbv1.ReloadRequest,
) (*pbv1.ReloadResponse, error) {
	s.log.Info("reload triggered", zap.String("initiated_by", req.InitiatedBy))

	if err := s.loader.Reload(); err != nil {
		return &pbv1.ReloadResponse{
			Success: false,
			Error:   err.Error(),
		}, nil
	}

	_, rawYAML, version := s.loader.Current()
	reason := "admin reload by " + req.InitiatedBy
	s.BroadcastSnapshot(rawYAML, version, reason)

	return &pbv1.ReloadResponse{
		NewVersion: version,
		Success:    true,
	}, nil
}

// BroadcastSnapshot pushes a config snapshot to all connected subscribers
// concurrently. Subscribers that fail to receive are removed.
func (s *Server) BroadcastSnapshot(rawYAML string, version uint64, reason string) {
	snapshot := buildSnapshot(rawYAML, version, reason)

	s.mu.RLock()
	subs := make([]*subscriber, 0, len(s.subscribers))
	for _, sub := range s.subscribers {
		subs = append(subs, sub)
	}
	s.mu.RUnlock()

	if len(subs) == 0 {
		s.log.Info("no subscribers — snapshot queued for next connect",
			zap.Uint64("version", version),
		)
		return
	}

	var wg sync.WaitGroup
	failed := make(chan string, len(subs))

	for _, sub := range subs {
		wg.Add(1)
		go func(sub *subscriber) {
			defer wg.Done()
			if err := sub.stream.Send(snapshot); err != nil {
				s.log.Warn("failed to send snapshot to subscriber",
					zap.String("instance_id", sub.id),
					zap.Error(err),
				)
				failed <- sub.id
				close(sub.done)
			}
		}(sub)
	}

	wg.Wait()
	close(failed)

	// Remove failed subscribers
	s.mu.Lock()
	for id := range failed {
		delete(s.subscribers, id)
	}
	s.mu.Unlock()

	s.log.Info("snapshot broadcast complete",
		zap.Uint64("version", version),
		zap.String("reason", reason),
		zap.Int("subscribers", len(subs)),
	)
}

// ActiveSubscribers returns the count of currently connected data planes.
func (s *Server) ActiveSubscribers() int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return len(s.subscribers)
}

// ── gRPC listener ─────────────────────────────────────────────────────────────

// Listen starts the gRPC server on the given address.
// Returns when ctx is cancelled.
func Listen(ctx context.Context, addr string, svc *Server, log *zap.Logger) error {
	lis, err := net.Listen("tcp", addr)
	if err != nil {
		return err
	}

	grpcSrv := googlegrpc.NewServer(
		googlegrpc.KeepaliveParams(keepalive.ServerParameters{
			MaxConnectionIdle: 5 * time.Minute,
			Time:              30 * time.Second,
			Timeout:           10 * time.Second,
		}),
		googlegrpc.KeepaliveEnforcementPolicy(keepalive.EnforcementPolicy{
			MinTime:             15 * time.Second,
			PermitWithoutStream: true,
		}),
	)

	pbv1.RegisterControlPlaneServiceServer(grpcSrv, svc)

	log.Info("gRPC server listening", zap.String("addr", addr))

	// Graceful stop on context cancellation
	go func() {
		<-ctx.Done()
		grpcSrv.GracefulStop()
	}()

	return grpcSrv.Serve(lis)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

func buildSnapshot(rawYAML string, version uint64, reason string) *pbv1.ConfigSnapshot {
	return &pbv1.ConfigSnapshot{
		Version:     version,
		ConfigYaml:  rawYAML,
		Reason:      reason,
		GeneratedAt: time.Now().Unix(),
	}
}
